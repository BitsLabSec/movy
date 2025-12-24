use move_binary_format::file_format::Bytecode;
use move_trace_format::format::TraceEvent;
use move_vm_stack::Stack;
use serde_json::json;
use sui_types::effects::TransactionEffects;
use z3::{
    DeclKind,
    ast::{Ast, Dynamic, Int},
};

use movy_replay::tracer::{
    concolic::{ConcolicState, SymbolValue},
    oracle::SuiGeneralOracle,
};
use movy_types::{
    error::MovyError,
    input::MoveSequence,
    oracle::{OracleFinding, Severity},
};

use super::common::{format_vulnerability_info, to_module_func};

#[derive(Debug, Default, Clone, Copy)]
pub struct BoolJudgementOracle;

impl<T, S> SuiGeneralOracle<T, S> for BoolJudgementOracle {
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
            TraceEvent::BeforeInstruction {
                pc, instruction, ..
            } => {
                let stack_syms = &symbol_stack.stack;
                let current = current_function.and_then(to_module_func);
                let loss = match instruction {
                    Bytecode::Eq
                    | Bytecode::Neq
                    | Bytecode::Lt
                    | Bytecode::Le
                    | Bytecode::Gt
                    | Bytecode::Ge => {
                        let stack_len = stack_syms.len();
                        if stack_len < 2 {
                            return Ok(vec![]);
                        }
                        let rhs = &stack_syms[stack_len - 1];
                        let lhs = &stack_syms[stack_len - 2];
                        match (lhs, rhs) {
                            (SymbolValue::Value(l), SymbolValue::Value(r)) => {
                                int_has_variable(l) == Some(false)
                                    && int_has_variable(r) == Some(false)
                            }
                            _ => false,
                        }
                    }
                    _ => false,
                };
                if loss {
                    let info = format_vulnerability_info(
                        "Unnecessary bool judgement (two constants)",
                        current.as_ref(),
                        Some(*pc),
                    );
                    Ok(vec![OracleFinding {
                        oracle: "BoolJudgementOracle".to_string(),
                        severity: Severity::Minor,
                        extra: json!(info),
                    }])
                } else {
                    Ok(vec![])
                }
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

fn int_has_variable(expr: &Int) -> Option<bool> {
    let mut stack = vec![Dynamic::from(expr.clone())];
    let mut count = 0;
    while let Some(node) = stack.pop() {
        count += 1;
        if count > 10000 {
            return None;
        }
        if node.is_const()
            && let Ok(decl) = node.safe_decl()
            && decl.kind() == DeclKind::UNINTERPRETED
        {
            return Some(true);
        }
        stack.extend(node.children());
    }
    Some(false)
}
