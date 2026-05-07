use std::{collections::BTreeMap, fmt::Write, path::PathBuf, sync::Arc};

use color_eyre::eyre::eyre;
use libafl::{HasMetadata, state::HasRand};
use libafl_bolts::serdeany::SerdeAnyMap;
use movy_replay::{
    env::SuiTestingEnv,
    exec::SuiExecutor,
    tracer::{SelectiveTracer, TeeTracer, lcov::LineCoverageCollector, tree::TreeTracer},
};
use movy_sui::database::cache::CachedStore;
use movy_sui::lcov::LineCoverageMap;
use movy_types::{
    error::MovyError,
    input::{FunctionIdent, MoveSequence},
};
use sui_types::{
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

pub fn test<T>(
    env: SuiTestingEnv<Arc<CachedStore<T>>>,
    meta: FuzzMetadata,
    trace: bool,
    lcov: Option<(PathBuf, LineCoverageMap)>,
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

        let mut sequence = MoveSequence::default();
        let built = append_function(
            &mut state,
            &mut sequence,
            &function,
            BTreeMap::new(),
            BTreeMap::new(),
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
