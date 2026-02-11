use std::{borrow::Cow, collections::BTreeMap, fmt::Display, marker::PhantomData, ops::AddAssign};

use libafl::{
    HasMetadata,
    executors::{Executor, ExitKind, HasObservers},
    observers::{MapObserver, ObserversTuple, StdMapObserver},
    state::{HasExecutions, HasRand},
};
use libafl_bolts::tuples::{Handle, MatchNameRef, RefIndexable};
use log::trace;
use movy_replay::{
    db::{ObjectStoreInfo, ObjectStoreMintObject},
    event::{ModuleProvider, NotifierTracer},
    exec::{ExecutionTracedResults, SuiExecutor},
    tracer::{concolic::ConcolicState, fuzz::SuiFuzzTracer, op::Log, oracle::SuiGeneralOracle},
};
use movy_sui::database::cache::{CachedStore, ObjectSuiStoreCommit};
use movy_types::{
    input::{FunctionIdent, MoveAddress},
    oracle::{Event, OracleFinding},
};
use move_core_types::account_address::AccountAddress;
use movy_types::error::MovyError;
use serde::{Deserialize, Serialize};
use sui_types::{
    effects::TransactionEffectsAPI,
    execution_status::ExecutionStatus,
    storage::{BackingStore, ObjectStore},
};

use crate::{
    input::MoveInput,
    meta::HasFuzzMetadata,
    state::{ExtraNonSerdeFuzzState, HasExtraState, HasFuzzEnv},
};

pub const CODE_OBSERVER_NAME: &str = "code_observer";

pub struct FuzzModuleProvider<'a, E> {
    env: &'a E,
}

impl<'a, E> FuzzModuleProvider<'a, E>
where
    E: ObjectStore,
{
    pub fn new(env: &'a E) -> Self {
        Self { env }
    }
}

impl<'a, E> ModuleProvider for FuzzModuleProvider<'a, E>
where
    E: ObjectStore,
{
    fn get_module(
        &mut self,
        address: AccountAddress,
        name: &str,
    ) -> Result<Option<move_binary_format::CompiledModule>, MovyError> {
        use sui_types::base_types::ObjectID;

        let db = self.env;
        let package_id = ObjectID::from(address);

        let package_obj = match db.get_object(&package_id) {
            Some(obj) => obj,
            None => return Ok(None),
        };

        if let Some(pkg) = package_obj.data.try_as_package() {
            for (module_name, bytes) in pkg.serialized_module_map() {
                if module_name.as_str() == name {
                    let module = move_binary_format::CompiledModule::deserialize_with_defaults(bytes)?;
                    return Ok(Some(module));
                }
            }
        }

        Ok(None)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionExtraOutcome {
    pub logs: BTreeMap<FunctionIdent, Vec<Log>>,
    pub solver: ConcolicState,
    pub stage_idx: Option<usize>,
    pub success: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOutcome {
    pub events_verdict: ExitKind,
    pub events: Vec<Event>,
    #[serde(default)]
    pub allowed_success: bool,
    #[serde(default)]
    pub findings: Vec<OracleFinding>,
}

impl Display for ExecutionOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ExecutionOutcome {{ events_verdict: {:?}, allowed_success: {}, findings: {:?} }}",
            self.events_verdict, self.allowed_success, self.findings
        )
    }
}

#[derive(Debug, Clone)]
pub struct GlobalOutcome {
    pub exec: ExecutionOutcome,
    pub extra: ExecutionExtraOutcome,
}

pub struct SuiFuzzExecutor<T, OT, RT, I, S> {
    pub executor: SuiExecutor<T>,
    pub ob: OT,
    pub attacker: MoveAddress,
    pub oracles: RT,
    // pub minted_gas: Object,
    // pub log_tracer: Option<SuiLogTracer>,
    pub ph: PhantomData<(I, S)>,
    pub epoch: u64,
    pub epoch_ms: u64,
}

impl<T, OT, RT, I, S> HasObservers for SuiFuzzExecutor<T, OT, RT, I, S> {
    type Observers = OT;
    fn observers(&self) -> RefIndexable<&Self::Observers, Self::Observers> {
        RefIndexable::from(&self.ob)
    }
    fn observers_mut(&mut self) -> RefIndexable<&mut Self::Observers, Self::Observers> {
        RefIndexable::from(&mut self.ob)
    }
}

impl<EM, Z, T, OT, RT, I, S, E> Executor<EM, I, S, Z> for SuiFuzzExecutor<T, OT, RT, I, S>
where
    T: ObjectStore + BackingStore + ObjectSuiStoreCommit + ObjectStoreMintObject + ObjectStoreInfo,
    E: ObjectStore,
    OT: ObserversTuple<I, S>,
    RT: for<'a> SuiGeneralOracle<CachedStore<&'a T>, S>,
    I: MoveInput,
    S: HasRand
        + HasFuzzMetadata
        + HasExecutions
        + HasMetadata
        + HasFuzzEnv<Env = E>
        + HasExtraState<ExtraState = ExtraNonSerdeFuzzState<E>>,
{
    fn run_target(
        &mut self,
        _fuzzer: &mut Z,
        state: &mut S,
        _mgr: &mut EM,
        input: &I,
    ) -> Result<ExitKind, libafl::Error> {
        // Clear any pending outcome
        // state.extra_state_mut().extra = None;
        let epoch = state.fuzz_state().epoch;
        let epoch_ms = state.fuzz_state().epoch_ms;
        {
            let code_ob: &mut StdMapObserver<'_, u8, false> = self
                .ob
                .get_mut(&Handle::new(Cow::Borrowed(CODE_OBSERVER_NAME)))
                .expect("no code ob installed");
            code_ob[0] = 1;
        }

        let db = CachedStore::new(&self.executor.db);
        self.oracles.pre_execution(&db, state, input.sequence())?;

        trace!("Executing input: {}", input.sequence());
        state.executions_mut().add_assign(1);
        let gas_id = state.fuzz_state().gas_id;
        let provider = FuzzModuleProvider::new(&self.executor.db);
        let tracer = NotifierTracer::with_provider(
            SuiFuzzTracer::new(
                &mut self.ob,
                state,
                &mut self.oracles,
                CODE_OBSERVER_NAME,
            ),
            provider,
        );
        let result = self.executor.run_ptb_with_gas(
            input.sequence().to_ptb()?,
            epoch,
            epoch_ms,
            self.attacker.into(),
            gas_id.into(),
            Some(tracer),
        )?;

        let ExecutionTracedResults { results, tracer } = result;
        let effects = results.effects;
        let events = results.store.events.data.clone();
        db.commit_store(results.store, &effects)
            .map_err(|e| libafl::Error::unknown(format!("commit store failed: {e}")))?;

        let mut trace_outcome = tracer
            .expect("tracer should be present when tracing is enabled")
            .into_inner()
            .outcome();

        trace!("Execution finished with status: {:?}", effects.status());

        let (stage_idx, success) = match effects.status() {
            ExecutionStatus::Failure { command, .. } => (
                // command index may be out of bound when meeting non-aborted error
                if command.is_some_and(|c| c < input.sequence().commands.len()) {
                    command.clone()
                } else {
                    None
                },
                false,
            ),
            _ => (None, true),
        };
        if effects.status().is_err() {
            let code_ob: &mut StdMapObserver<'_, u8, false> = self
                .ob
                .get_mut(&Handle::new(Cow::Borrowed(CODE_OBSERVER_NAME)))
                .expect("no code ob installed");
            code_ob.reset_map()?;
        }
        let extra = ExecutionExtraOutcome {
            logs: trace_outcome.logs,
            solver: trace_outcome.concolic,
            stage_idx,
            success,
        };

        if log::log_enabled!(log::Level::Debug) {
            for ev in events.iter() {
                if let Some((st, ev)) = state.fuzz_state().decode_sui_event(ev)? {
                    log::debug!(
                        "Event: {}({})",
                        st.to_canonical_string(true),
                        serde_json::to_string(&ev)
                            .unwrap_or_else(|e| format!("json err({}): {:?}", e, ev))
                    );
                } else {
                    log::debug!(
                        "Event {} missing for decoding",
                        ev.type_.to_canonical_string(true)
                    );
                }
            }
        }
        let events: Vec<_> = events.into_iter().map(|e| e.into()).collect();

        // Expose preliminary outcome so oracles can inspect events.
        let mut exec = ExecutionOutcome {
            events_verdict: trace_outcome.verdict,
            events: events.clone(),
            allowed_success: success,
            findings: trace_outcome.findings.clone(),
        };
        state.extra_state_mut().global_outcome = Some(GlobalOutcome {
            exec: exec.clone(),
            extra: extra.clone(),
        });

        let oracle_vulns = self.oracles.done_execution(&db, state, &effects)?;
        if !oracle_vulns.is_empty() {
            trace_outcome.findings.extend(oracle_vulns.iter().cloned());
        }
        let kind = if !oracle_vulns.is_empty() {
            ExitKind::Crash
        } else {
            trace_outcome.verdict
        };
        exec = ExecutionOutcome {
            events_verdict: kind,
            events,
            allowed_success: success,
            findings: trace_outcome.findings.clone(),
        };
        state.extra_state_mut().global_outcome = Some(GlobalOutcome { exec, extra });

        // if let Some(tracer) = &self.log_tracer
        //     && let Some(v) = tracer.may_log(input, &outcome, &extra)
        // {
        //     info!("{}", v);
        // }

        Ok(kind)
    }
}
