use move_trace_format::format::TraceEvent;
use move_vm_stack::Stack;
use serde_json::json;

use movy_replay::tracer::{
    concolic::{ConcolicState, SymbolValue},
    oracle::SuiGeneralOracle,
};
use movy_types::{error::MovyError, input::MoveSequence, oracle::OracleFinding};
use sui_types::effects::TransactionEffects;
use z3::ast::Ast;

#[derive(Debug, Default, Clone, Copy)]
pub struct PrecisionLossOracle;

impl<T, S> SuiGeneralOracle<T, S> for PrecisionLossOracle {
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
                let loss = match instruction {
                    move_binary_format::file_format::Bytecode::Mul => {
                        let stack_len = symbol_stack.stack.len();
                        if stack_len < 2 {
                            return Ok(vec![]);
                        }
                        let rhs = &symbol_stack.stack[stack_len - 1];
                        let lhs = &symbol_stack.stack[stack_len - 2];
                        match (lhs, rhs) {
                            (SymbolValue::Value(l), _) => contains_division(l),
                            (_, SymbolValue::Value(r)) => contains_division(r),
                            _ => false,
                        }
                    }
                    _ => false,
                };
                if loss {
                    let info = json!({
                        "oracle": "PrecisionLossOracle",
                        "function": current_function.as_ref().map(|f| f.to_string()),
                        "pc": pc,
                    });
                    Ok(vec![OracleFinding {
                        oracle: "PrecisionLossOracle".to_string(),
                        severity: movy_types::oracle::Severity::Medium,
                        extra: info,
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

fn contains_division(expr: &z3::ast::Int) -> bool {
    let mut stack = vec![z3::ast::Dynamic::from(expr.clone())];
    let mut count = 0;
    while let Some(node) = stack.pop() {
        count += 1;
        if count > 10000 {
            break;
        }
        if let Ok(decl) = node.safe_decl() {
            match decl.kind() {
                z3::DeclKind::DIV | z3::DeclKind::IDIV => return true,
                _ => {}
            }
        }
        stack.extend(node.children());
    }
    false
}
