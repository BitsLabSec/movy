use move_binary_format::file_format::Bytecode;
use move_core_types::u256::U256;
use move_trace_format::format::{TraceEvent, TraceValue};
use serde_json::json;

use movy_replay::tracer::{
    concolic::{ConcolicState, value_bitwidth, value_to_u256},
    oracle::SuiGeneralOracle,
    trace::TraceState,
};
use movy_types::{
    error::MovyError,
    input::MoveSequence,
    oracle::{OracleFinding, Severity},
};
use sui_types::effects::TransactionEffects;

#[derive(Debug, Default, Clone, Copy)]
pub struct OverflowOracle;

/// Count the number of significant bits in the concrete value (0 => 0 bits).
fn value_sig_bits(v: &TraceValue) -> u32 {
    let as_u256 = value_to_u256(v);
    if as_u256 == U256::zero() {
        0
    } else {
        256 - as_u256.leading_zeros()
    }
}

impl<T, S> SuiGeneralOracle<T, S> for OverflowOracle {
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
        trace_state: &TraceState,
        _symbol_stack: &ConcolicState,
        current_function: Option<&movy_types::input::FunctionIdent>,
        _state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        match event {
            TraceEvent::Instruction {
                pc, instruction, ..
            } => {
                if !matches!(instruction, Bytecode::Shl) {
                    return Ok(vec![]);
                }
                let stack = &trace_state.operand_stack;
                if stack.len() < 2 {
                    return Ok(vec![]);
                }
                let lhs = &stack[stack.len() - 2];
                let rhs = &stack[stack.len() - 1];
                let lhs_width_bits = value_bitwidth(lhs); // type width (u8/u16/...)
                let lhs_sig_bits = value_sig_bits(lhs); // actual significant bits of the value
                let rhs_bits = value_to_u256(rhs);

                let overflow = if rhs_bits >= U256::from(lhs_width_bits) {
                    true
                } else {
                    let shift = rhs_bits.unchecked_as_u32();
                    // If shifting the current significant bits would cross the type width, it's an overflow.
                    lhs_sig_bits + shift > lhs_width_bits
                };

                if overflow {
                    let info = json!({
                        "oracle": "OverflowOracle",
                        "function": current_function.as_ref().map(|f| f.to_string()),
                        "pc": pc,
                    });
                    return Ok(vec![OracleFinding {
                        oracle: "OverflowOracle".to_string(),
                        severity: Severity::Medium,
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
