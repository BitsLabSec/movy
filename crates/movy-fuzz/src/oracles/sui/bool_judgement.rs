use move_binary_format::file_format::Bytecode;
use serde_json::json;
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
    oracle::{OracleFinding, Severity},
};

use super::common::{format_vulnerability_info, to_module_func};

#[derive(Debug, Default, Clone, Copy)]
pub struct BoolJudgementOracle;

impl<S> SuiGeneralOracle<S> for BoolJudgementOracle {
    fn before_instruction(
        &mut self,
        pc: u16,
        instruction: &Bytecode,
        _trace_state: &movy_replay::tracer::state::TraceState,
        symbol_stack: &ConcolicState,
        current_function: &movy_types::input::FunctionIdent,
        _state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        let stack_syms = &symbol_stack.stack;
        let current = to_module_func(current_function);
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
                        int_has_variable(l) == Some(false) && int_has_variable(r) == Some(false)
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
                Some(pc),
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
            && decl.kind() == DeclKind::Uninterpreted
        {
            return Some(true);
        }
        stack.extend(node.children());
    }
    Some(false)
}
