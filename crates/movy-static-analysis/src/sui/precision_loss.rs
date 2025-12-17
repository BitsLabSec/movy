use move_model::symbol::SymbolPool;
use move_stackless_bytecode::stackless_bytecode::{Bytecode as SLBytecode, Operation};
use serde_json::json;

use super::{common::ModuleAnalysis, generate_bytecode::FunctionInfo};
use movy_types::oracle::{OracleFinding, Severity};

pub fn analyze(modules: &[ModuleAnalysis]) -> Vec<OracleFinding> {
    let mut reports = Vec::new();

    for module in modules {
        for function in module.functions() {
            if module.is_native(function) {
                continue;
            }
            if detect_precision_loss(function, module.global_env.symbol_pool()) {
                reports.push(OracleFinding {
                    oracle: "StaticPrecisionLoss".to_string(),
                    severity: Severity::Medium,
                    extra: json!({
                        "module": module.qualified_module_name(),
                        "function": function.name.clone(),
                        "message": "Potential precision loss from multiplication involving division/sqrt"
                    }),
                });
            }
        }
    }

    reports
}

fn detect_precision_loss(function: &FunctionInfo, symbol_pool: &SymbolPool) -> bool {
    for (offset, instr) in function.code.iter().enumerate() {
        if let SLBytecode::Call(_, _, Operation::Mul, srcs, _) = instr {
            if srcs.len() != 2 {
                continue;
            }
            let op1 = match super::common::get_def_bytecode(function, srcs[0], offset) {
                Some(code) => code,
                None => continue,
            };
            let op2 = match super::common::get_def_bytecode(function, srcs[1], offset) {
                Some(code) => code,
                None => continue,
            };
            if is_div(op1) || is_div(op2) || is_sqrt(op1, symbol_pool) || is_sqrt(op2, symbol_pool)
            {
                return true;
            }
        }
    }
    false
}

fn is_div(bytecode: &SLBytecode) -> bool {
    matches!(bytecode, SLBytecode::Call(_, _, Operation::Div, _, _))
}

fn is_sqrt(bytecode: &SLBytecode, symbol_pool: &SymbolPool) -> bool {
    matches!(bytecode, SLBytecode::Call(_, _, Operation::Function(_, fid, _), _, _) if symbol_pool.string(fid.symbol()).as_str() == "sqrt")
}
