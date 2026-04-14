use serde_json::json;

use movy_replay::tracer::{
    concolic::{ConcolicState, SymbolValue},
    oracle::SuiGeneralOracle,
};
use movy_types::{error::MovyError, oracle::OracleFinding};
use z3::ast::Ast;

#[derive(Debug, Default, Clone, Copy)]
pub struct PrecisionLossOracle;

impl<S> SuiGeneralOracle<S> for PrecisionLossOracle {
    fn before_instruction(
        &mut self,
        pc: u16,
        instruction: &move_binary_format::file_format::Bytecode,
        _trace_state: &movy_replay::tracer::state::TraceState,
        symbol_stack: &ConcolicState,
        current_function: &movy_types::input::FunctionIdent,
        _state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError> {
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
        if matches!(instruction, move_binary_format::file_format::Bytecode::Mul)
            && ["flash_loan", "full_math_u64"]
                .iter()
                .any(|needle| current_function.to_string().contains(needle))
        {
            tracing::info!(
                "precision oracle saw Mul in {} at pc {}: contains_division={}",
                current_function,
                pc,
                loss
            );
        }
        if loss {
            let info = json!({
                "oracle": "PrecisionLossOracle",
                "function": current_function.to_string(),
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
