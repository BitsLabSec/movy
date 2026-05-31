use std::{
    collections::{BTreeMap, VecDeque},
    fmt::Write,
    path::PathBuf,
    sync::Arc,
};

use color_eyre::eyre::eyre;
use libafl::{HasMetadata, state::HasRand};
use libafl_bolts::serdeany::SerdeAnyMap;
use movy_replay::{
    db::ObjectStoreInfo,
    env::SuiTestingEnv,
    exec::SuiExecutor,
    tracer::{SelectiveTracer, TeeTracer, lcov::LineCoverageCollector, tree::TreeTracer},
};
use movy_sui::database::cache::CachedStore;
use movy_sui::lcov::LineCoverageMap;
use movy_types::{
    abi::{MoveAbiSignatureToken, MoveFunctionAbi},
    error::MovyError,
    input::{
        FunctionIdent, InputArgument, MoveAddress, MoveSequence, MoveTypeTag, SequenceArgument,
        SuiObjectInputArgument,
    },
    object::MoveOwner,
};
use sui_types::{
    digests::TransactionDigest,
    effects::TransactionEffectsAPI,
    storage::{BackingPackageStore, BackingStore, ObjectStore},
};

use crate::{
    meta::{FuzzMetadata, HasFuzzMetadata},
    mutators::sequence::{append_function, apply_hooks},
    state::{ExtraNonSerdeFuzzState, HasExtraState, HasFuzzEnv},
    utils::SuperRand,
};

struct SingleRunState<T> {
    rand: SuperRand,
    metadata: SerdeAnyMap,
    extra: ExtraNonSerdeFuzzState<T>,
}

impl<T> SingleRunState<T> {
    fn new(rand: SuperRand, env: SuiTestingEnv<T>) -> Self {
        Self {
            rand,
            metadata: SerdeAnyMap::default(),
            extra: ExtraNonSerdeFuzzState::from_env(env),
        }
    }
}

impl<T> HasMetadata for SingleRunState<T> {
    fn metadata_map(&self) -> &SerdeAnyMap {
        &self.metadata
    }

    fn metadata_map_mut(&mut self) -> &mut SerdeAnyMap {
        &mut self.metadata
    }
}

impl<T> HasRand for SingleRunState<T> {
    type Rand = SuperRand;

    fn rand(&self) -> &Self::Rand {
        &self.rand
    }

    fn rand_mut(&mut self) -> &mut Self::Rand {
        &mut self.rand
    }
}

impl<T> HasExtraState for SingleRunState<T> {
    type ExtraState = ExtraNonSerdeFuzzState<T>;

    fn extra_state(&self) -> &Self::ExtraState {
        &self.extra
    }

    fn extra_state_mut(&mut self) -> &mut Self::ExtraState {
        &mut self.extra
    }
}

fn format_execution_failure(
    function: &FunctionIdent,
    sequence: &MoveSequence,
    trace: Option<&str>,
    status: &dyn std::fmt::Debug,
) -> String {
    let mut out =
        format!("test execution failed for {function}\nstatus: {status:?}\nsequence:\n{sequence}");
    if let Some(trace) = trace.filter(|trace| !trace.trim().is_empty()) {
        let _ = write!(out, "\ntrace:\n{trace}");
    }
    out
}

/// Determine the concrete object type a parameter expects, after substituting `ty_args`, along
/// with whether it is passed as an immutable reference (`&T`). Returns `None` for non-object
/// parameters or generics that are not fully resolved by `ty_args`.
fn object_param_info(
    param: &MoveAbiSignatureToken,
    ty_args: &BTreeMap<u16, MoveTypeTag>,
) -> Option<(MoveTypeTag, bool)> {
    match param {
        MoveAbiSignatureToken::Struct { .. } | MoveAbiSignatureToken::StructInstantiation(_, _) => {
            param.subst(ty_args).map(|ty| (ty, false))
        }
        MoveAbiSignatureToken::Reference(inner) => match inner.as_ref() {
            MoveAbiSignatureToken::Struct { .. }
            | MoveAbiSignatureToken::StructInstantiation(_, _) => {
                inner.subst(ty_args).map(|ty| (ty, true))
            }
            _ => None,
        },
        MoveAbiSignatureToken::MutableReference(inner) => match inner.as_ref() {
            MoveAbiSignatureToken::Struct { .. }
            | MoveAbiSignatureToken::StructInstantiation(_, _) => {
                inner.subst(ty_args).map(|ty| (ty, false))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Build the explicit (fixed) arguments for a test function from `--object-mapping`. For each
/// object parameter whose type has a mapped object id (consumed in parameter order), inject the
/// object as a PTB input and bind it to that parameter. Unmapped parameters are left for
/// `append_function` to fill.
fn build_test_fixed_args(
    store: &impl ObjectStoreInfo,
    func: &MoveFunctionAbi,
    sequence: &mut MoveSequence,
    remaining: &mut BTreeMap<MoveTypeTag, VecDeque<MoveAddress>>,
    fixed_ty_args: &BTreeMap<u16, MoveTypeTag>,
) -> Result<BTreeMap<u16, (SequenceArgument, MoveTypeTag)>, MovyError> {
    let mut fixed = BTreeMap::new();
    for (i, param) in func.parameters.iter().enumerate() {
        if param.is_tx_context() {
            continue;
        }
        let Some((ty, immutable_ref)) = object_param_info(param, fixed_ty_args) else {
            continue;
        };
        let Some(queue) = remaining.get_mut(&ty) else {
            continue;
        };
        let Some(id) = queue.pop_front() else {
            continue;
        };
        let info = store.get_move_object_info(id)?;
        let input = match info.owner {
            MoveOwner::AddressOwner(_) | MoveOwner::Immutable => {
                let digest: TransactionDigest = info.digest.into();
                InputArgument::Object(
                    info.ty.clone(),
                    SuiObjectInputArgument::imm_or_owned_object(
                        id,
                        info.version,
                        digest.into_inner(),
                    ),
                )
            }
            MoveOwner::Shared {
                initial_shared_version,
            } => InputArgument::Object(
                info.ty.clone(),
                SuiObjectInputArgument::shared_object(id, initial_shared_version, !immutable_ref),
            ),
            other => {
                return Err(
                    eyre!("--object-mapping object {id} has unsupported owner {other:?}").into(),
                );
            }
        };
        sequence.inputs.push(input);
        let arg = SequenceArgument::Input((sequence.inputs.len() - 1) as u16);
        fixed.insert(i as u16, (arg, ty));
    }
    Ok(fixed)
}

/// Decode the human-readable reason carried by a `movy::oracle::Crash` event. The event wraps a
/// `movy::log::Log` (a list of optionally-keyed strings); `crash_because` stores the message under
/// the `reason` key. Returns `None` if the payload doesn't decode or carries no message.
fn decode_crash_reason(contents: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Crash {
        reason: Log,
    }
    #[derive(serde::Deserialize)]
    struct Log {
        msg: Vec<MayKeyedString>,
    }
    #[derive(serde::Deserialize)]
    struct MayKeyedString {
        key: Option<String>,
        value: String,
    }

    let crash: Crash = bcs::from_bytes(contents).ok()?;
    if let Some(entry) = crash
        .reason
        .msg
        .iter()
        .find(|entry| entry.key.as_deref() == Some("reason"))
    {
        return Some(entry.value.clone());
    }
    if crash.reason.msg.is_empty() {
        return None;
    }
    Some(
        crash
            .reason
            .msg
            .iter()
            .map(|entry| match &entry.key {
                Some(key) => format!("{key}={}", entry.value),
                None => entry.value.clone(),
            })
            .collect::<Vec<_>>()
            .join(", "),
    )
}

pub fn test<T>(
    env: SuiTestingEnv<Arc<CachedStore<T>>>,
    meta: FuzzMetadata,
    trace: bool,
    lcov: Option<(PathBuf, LineCoverageMap)>,
    object_mapping: BTreeMap<MoveTypeTag, Vec<MoveAddress>>,
    type_args: BTreeMap<FunctionIdent, BTreeMap<u16, MoveTypeTag>>,
) -> Result<(), MovyError>
where
    T: ObjectStore + BackingStore + BackingPackageStore + Clone + 'static,
{
    let target_functions = meta.target_functions.clone();
    if target_functions.is_empty() {
        return Err(eyre!("no target functions selected").into());
    }

    let mut state = SingleRunState::new(meta.rand.clone(), env);
    state.add_metadata::<FuzzMetadata>(meta);

    let baseline = state.fuzz_env().inner().dump_snapshot();
    let executor = SuiExecutor::new(state.fuzz_env().inner().clone())?;
    let attacker = state.fuzz_state().attacker;
    let epoch = state.fuzz_state().epoch;
    let epoch_ms = state.fuzz_state().epoch_ms;
    let gas_id = state.fuzz_state().gas_id;

    let coverage = lcov.map(|(path, map)| (path, map, LineCoverageCollector::new()));

    for function in target_functions {
        println!("running {function}");
        state.fuzz_env().inner().reset();
        state.fuzz_env().inner().restore_snapshot(baseline.clone());

        let fixed_ty_args = type_args.get(&function).cloned().unwrap_or_default();
        let func_abi = state
            .fuzz_state()
            .get_function(
                &function.0.module_address,
                &function.0.module_name,
                &function.1,
            )
            .cloned();

        let mut sequence = MoveSequence::default();
        let fixed_args = if let Some(func_abi) = &func_abi {
            let mut remaining: BTreeMap<MoveTypeTag, VecDeque<MoveAddress>> = object_mapping
                .iter()
                .map(|(ty, ids)| (ty.clone(), ids.iter().copied().collect()))
                .collect();
            build_test_fixed_args(
                state.fuzz_env().inner(),
                func_abi,
                &mut sequence,
                &mut remaining,
                &fixed_ty_args,
            )?
        } else {
            BTreeMap::new()
        };

        let built = append_function(
            &mut state,
            &mut sequence,
            &function,
            fixed_args,
            fixed_ty_args,
            &vec![],
            false,
            0,
        );
        if built.is_none() {
            return Err(eyre!("unable to construct a test sequence for {function}").into());
        }

        let sequence = apply_hooks(&mut state, &sequence);
        let tracer = if let Some((_, _, collector)) = &coverage {
            SelectiveTracer::T1(TeeTracer(TreeTracer::new(), collector.tracer()))
        } else {
            SelectiveTracer::T2(TreeTracer::new())
        };
        let results = executor.run_ptb_with_movy_testing_tracer_gas(
            sequence.to_ptb()?,
            epoch,
            epoch_ms,
            attacker.into(),
            gas_id.into(),
            Some(tracer),
        )?;
        let trace_output = results.tracer.map(|tracer| match tracer {
            SelectiveTracer::T1(TeeTracer(tree, _)) => tree.take_inner().pprint_failure_views(),
            SelectiveTracer::T2(tree) => tree.take_inner().pprint_failure_views(),
        });

        if !results.results.effects.status().is_ok() {
            return Err(eyre!(
                "{}",
                format_execution_failure(
                    &function,
                    &sequence,
                    trace_output.as_deref(),
                    results.results.effects.status(),
                )
            )
            .into());
        }

        // movy oracles (movy_pre_*/movy_post_*) report invariant violations by emitting a
        // movy::oracle::Crash event rather than aborting, so a successful transaction status is
        // not sufficient: any crash event emitted while running the test is a failure.
        let crash_event = results.results.store.events.data.iter().find(|event| {
            event.type_.module.as_str() == "oracle" && event.type_.name.as_str() == "Crash"
        });
        if let Some(crash_event) = crash_event {
            let mut message = match decode_crash_reason(&crash_event.contents) {
                Some(reason) => format!("oracle crash detected for {function}: {reason}"),
                None => format!("oracle crash detected for {function}"),
            };
            if let Some(trace) = trace_output
                .as_deref()
                .filter(|trace| !trace.trim().is_empty())
            {
                let _ = write!(message, "\ntrace:\n{trace}");
            }
            return Err(eyre!("{}", message).into());
        }

        if trace && let Some(trace_output) = trace_output {
            println!("trace for {function}:\n{trace_output}");
        }
        println!("ok {function}");
    }

    if let Some((path, map, collector)) = coverage {
        map.write_lcov(collector.hits(), &path)?;
    }

    Ok(())
}
