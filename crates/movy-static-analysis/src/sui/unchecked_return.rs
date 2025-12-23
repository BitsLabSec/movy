use move_model::model::FunId;
use move_stackless_bytecode::stackless_bytecode::{Bytecode, Operation};
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
            for fid in detect_unchecked_return(function) {
                reports.push(OracleFinding {
                    oracle: "StaticUncheckedReturn".to_string(),
                    severity: Severity::Minor,
                    extra: json!({
                        "module": module.qualified_module_name(),
                        "function": function.name.clone(),
                        "callee": module.get_function_name(&fid),
                        "message": "Return value dropped without handling"
                    }),
                });
            }
        }
    }

    reports
}

fn detect_unchecked_return(function: &FunctionInfo) -> Vec<FunId> {
    let mut fids = vec![];
    for (offset, instr) in function.code.iter().enumerate() {
        if let Bytecode::Call(_, dsts, Operation::Function(_, fid, _), _, _) = instr {
            if dsts.is_empty() {
                continue;
            }
            let mut consumed = 0usize;
            let mut idx = offset + 1;
            while idx < function.code.len() && consumed < dsts.len() {
                if let Bytecode::Call(_, _, Operation::Destroy, srcs, _) = &function.code[idx]
                    && srcs.len() == 1
                    && dsts.contains(&srcs[0])
                {
                    consumed += 1;
                    break;
                }
                idx += 1;
            }
            if consumed > 0 {
                fids.push(*fid);
            }
        }
    }
    fids
}
