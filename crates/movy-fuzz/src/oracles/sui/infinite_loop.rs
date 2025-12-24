use std::collections::BTreeMap;

use move_binary_format::file_format::Bytecode;
use move_trace_format::format::TraceEvent;
use move_vm_stack::Stack;
use serde_json::json;

use movy_replay::tracer::{
    concolic::{ConcolicState, SymbolValue},
    oracle::SuiGeneralOracle,
};
use movy_types::{
    error::MovyError,
    input::MoveSequence,
    oracle::{OracleFinding, Severity},
};
use sui_types::effects::TransactionEffects;

use crate::utils::hash_to_u64;

use super::common::to_module_func;

#[derive(Debug, Default, Clone)]
pub struct InfiniteLoopOracle {
    pub branch_counts: BTreeMap<u64, BTreeMap<u16, (u64, usize)>>,
}

impl<T, S> SuiGeneralOracle<T, S> for InfiniteLoopOracle {
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
        _stack: Option<&Stack>,
        symbol_stack: &ConcolicState,
        current_function: Option<&movy_types::input::FunctionIdent>,
        _state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        match event {
            TraceEvent::OpenFrame { frame, .. } => {
                let key = format!("{}::{}", frame.module, frame.function_name);
                let key = hash_to_u64(&key);
                self.branch_counts.remove(&key);
            }
            TraceEvent::BeforeInstruction {
                pc, instruction, ..
            } => {
                match instruction {
                    Bytecode::BrFalse(_) | Bytecode::BrTrue(_) => {
                        let Some(func) = current_function.and_then(to_module_func) else {
                            return Ok(vec![]);
                        };
                        if symbol_stack.stack.is_empty() {
                            return Ok(vec![]);
                        }
                        let cond_symbol = &symbol_stack.stack[symbol_stack.stack.len() - 1];
                        match cond_symbol {
                            SymbolValue::Unknown => return Ok(vec![]),
                            SymbolValue::Value(v) => {
                                let key = format!("{}::{}", func.0, func.1);
                                let key = hash_to_u64(&key);
                                let v = hash_to_u64(&v.to_string());
                                let count = self
                                    .branch_counts
                                    .entry(key)
                                    .or_default()
                                    .entry(*pc)
                                    .or_default();
                                if count.0 != v {
                                    count.0 = v;
                                    count.1 = 1;
                                } else {
                                    if count.1 >= 1000 {
                                        count.1 = 0;
                                        let info = json!({
                                            "oracle": "InfiniteLoopOracle",
                                            "function": current_function.as_ref().map(|f| f.to_string()),
                                            "pc": pc,
                                        });
                                        return Ok(vec![OracleFinding {
                                            oracle: "InfiniteLoopOracle".to_string(),
                                            severity: Severity::Major,
                                            extra: info,
                                        }]);
                                    }
                                    count.1 += 1;
                                }
                            }
                        }
                    }
                    _ => {}
                };
            }
            _ => {}
        }
        Ok(vec![])
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
