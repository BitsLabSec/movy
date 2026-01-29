use log::{debug, trace};
use move_trace_format::{format::TraceEvent};

use movy_replay::tracer::{concolic::ConcolicState, oracle::SuiGeneralOracle, trace::TraceState};
use movy_types::{error::MovyError, input::MoveSequence, oracle::OracleFinding};
use serde_json::json;
use sui_types::{
    effects::{TransactionEffects, TransactionEffectsAPI},
    execution_status::{ExecutionFailureStatus, ExecutionStatus},
    storage::ObjectStore,
};

use crate::{
    meta::HasFuzzMetadata,
    state::{ExtraNonSerdeFuzzState, HasExtraState},
};

const TYPED_BUG_ABORT_CODE: u64 = 19260817;

#[derive(Debug, Clone)]
pub struct TypedBugOracle {
    pub use_abort: bool,
}

impl Default for TypedBugOracle {
    fn default() -> Self {
        Self { use_abort: false }
    }
}

impl TypedBugOracle {
    pub fn new(use_abort: bool) -> Self {
        Self { use_abort }
    }
}

impl<T, S, E> SuiGeneralOracle<T, S> for TypedBugOracle
where
    S: HasExtraState<ExtraState = ExtraNonSerdeFuzzState<E>> + HasFuzzMetadata,
    T: ObjectStore,
{
    fn pre_execution(
        &mut self,
        _db: &T,
        _state: &mut S,
        _sequence: &MoveSequence,
    ) -> Result<(), MovyError> {
        Ok(())
    }

    fn event(
        &mut self,
        _event: &TraceEvent,
        _trace_state: &TraceState,
        _symbol_stack: &ConcolicState,
        _current_function: Option<&movy_types::input::FunctionIdent>,
        _state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        Ok(vec![])
    }

    fn done_execution(
        &mut self,
        _db: &T,
        state: &mut S,
        _effects: &TransactionEffects,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        trace!("TypedBugOracle done_execution called");
        if self.use_abort {
            match _effects.status() {
                ExecutionStatus::Failure {
                    error: ExecutionFailureStatus::MoveAbort(_, code),
                    ..
                } if *code == TYPED_BUG_ABORT_CODE => {
                    debug!("Typed bug abort detected: code {}", code);
                    return Ok(vec![OracleFinding {
                        oracle: "TypedBugOracle".to_string(),
                        severity: movy_types::oracle::Severity::Critical,
                        extra: json!({
                            "abort_code": code,
                        }),
                    }]);
                }
                _ => return Ok(Vec::new()),
            }
        }
        let Some(global_outcome) = state.extra_state().global_outcome.as_ref() else {
            return Ok(Vec::new());
        };
        if !global_outcome.exec.allowed_success {
            return Ok(Vec::new());
        }
        for event in &global_outcome.exec.events {
            if event.ty.module == "oracle" && event.ty.name == "Crash" {
                debug!("Typed bug event detected: {:?}", event);
                return Ok(vec![OracleFinding {
                    oracle: "TypedBugOracle".to_string(),
                    severity: movy_types::oracle::Severity::Critical,
                    extra: json!({
                        "event": event,
                    }),
                }]);
            }
        }
        Ok(vec![])
    }
}
