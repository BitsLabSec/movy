use move_binary_format::file_format::Bytecode;
use move_trace_format::format::TraceEvent;
use move_vm_stack::Stack;
use movy_types::input::MoveSequence;
use movy_types::oracle::OracleFinding;
use serde_json::json;

use movy_replay::tracer::concolic::value_bitwidth;
use movy_replay::tracer::{concolic::ConcolicState, oracle::SuiGeneralOracle};
use movy_types::error::MovyError;
use sui_types::effects::TransactionEffects;

#[derive(Debug, Default, Clone, Copy)]
pub struct TypeConversionOracle;

impl<T, S> SuiGeneralOracle<T, S> for TypeConversionOracle {
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
        event: &TraceEvent,
        stack: Option<&Stack>,
        _symbol_stack: &ConcolicState,
        current_function: Option<movy_types::input::FunctionIdent>,
        _state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        match event {
            TraceEvent::BeforeInstruction {
                pc, instruction, ..
            } => {
                let stack = match stack {
                    Some(s) => s,
                    None => return Ok(vec![]),
                };
                let Ok(vals_iter) = stack.last_n(1) else {
                    return Ok(vec![]);
                };
                let vals: Vec<_> = vals_iter.collect();
                let val = vals.first().unwrap();
                let unnecessary = match instruction {
                    Bytecode::CastU8 => value_bitwidth(val) == 8,
                    Bytecode::CastU16 => value_bitwidth(val) == 16,
                    Bytecode::CastU32 => value_bitwidth(val) == 32,
                    Bytecode::CastU64 => value_bitwidth(val) == 64,
                    Bytecode::CastU128 => value_bitwidth(val) == 128,
                    Bytecode::CastU256 => value_bitwidth(val) == 256,
                    _ => false,
                };
                if unnecessary {
                    let info = json!({
                        "oracle": "TypeConversionOracle",
                        "function": current_function.as_ref().map(|f| f.to_string()),
                        "pc": pc,
                    });
                    return Ok(vec![OracleFinding {
                        oracle: "TypeConversionOracle".to_string(),
                        severity: movy_types::oracle::Severity::Minor,
                        extra: info,
                    }]);
                }
                Ok(vec![])
            }
            _ => Ok(vec![]),
        }
    }

    fn done_execution(
        &mut self,
        _db: &T,
        _state: &mut S,
        _effects: &TransactionEffects,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        Ok(vec![])
    }
}
