use log::debug;
use move_trace_format::format::TraceEvent;
use move_vm_stack::Stack;

use movy_replay::tracer::{concolic::ConcolicState, oracle::SuiGeneralOracle};
use movy_types::{error::MovyError, input::MoveSequence, oracle::OracleFinding};
use serde_json::json;
use sui_types::{effects::TransactionEffects, storage::ObjectStore};

use crate::{
    meta::HasFuzzMetadata,
    state::{ExtraNonSerdeFuzzState, HasExtraState},
};

#[derive(Debug, Default, Clone)]
pub struct TypedBugOracle;

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
        _stack: Option<&Stack>,
        _symbol_stack: &ConcolicState,
        _current_function: Option<movy_types::input::FunctionIdent>,
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
