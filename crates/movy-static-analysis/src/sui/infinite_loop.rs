use move_stackless_bytecode::stackless_bytecode::Bytecode as SLBytecode;
use serde_json::json;

use super::{
    common::{ModuleAnalysis, get_def_bytecode},
    generate_bytecode::FunctionInfo,
};
use movy_types::oracle::{OracleFinding, Severity};

pub fn analyze(modules: &[ModuleAnalysis]) -> Vec<OracleFinding> {
    let mut reports = Vec::new();

    for module in modules {
        for function in module.functions() {
            if module.is_native(function) {
                continue;
            }
            if detect_infinite_loop(function) {
                reports.push(OracleFinding {
                    oracle: "StaticInfiniteLoop".to_string(),
                    severity: Severity::Major,
                    extra: json!({
                        "module": module.qualified_module_name(),
                        "function": function.name.clone(),
                        "message": "Potential infinite loop detected from constant branch condition"
                    }),
                });
            }
        }
    }

    reports
}

fn detect_infinite_loop(function: &FunctionInfo) -> bool {
    use move_binary_format::file_format::CodeOffset;
    let label_offsets = SLBytecode::label_offsets(&function.code);
    for (offset, instr) in function.code.iter().enumerate() {
        if let SLBytecode::Branch(_, then_label, else_label, cond) = instr {
            let Some(def_instr) = get_def_bytecode(function, *cond, offset) else {
                continue;
            };
            let constant = match def_instr {
                SLBytecode::Load(
                    _,
                    _,
                    move_stackless_bytecode::stackless_bytecode::Constant::Bool(v),
                ) => Some(*v),
                _ => None,
            };
            let Some(value) = constant else {
                continue;
            };
            let current_offset = offset as CodeOffset;
            let then_offset = match label_offsets.get(then_label) {
                Some(v) => *v,
                None => continue,
            };
            let else_offset = match label_offsets.get(else_label) {
                Some(v) => *v,
                None => continue,
            };

            if value && then_offset <= current_offset {
                return true;
            }
            if !value && else_offset <= current_offset {
                return true;
            }
        }
    }
    false
}
