use move_model::ty::{PrimitiveType, Type};
use move_stackless_bytecode::stackless_bytecode::{Bytecode as SLBytecode, Constant, Operation};
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
            if detect_bool_judgement(function) {
                reports.push(OracleFinding {
                    oracle: "StaticBoolJudgement".to_string(),
                    severity: Severity::Minor,
                    extra: json!({
                        "module": module.qualified_module_name(),
                        "function": function.name.clone(),
                        "message": "Unnecessary bool judgement (boolean compared with boolean literal)"
                    }),
                });
            }
        }
    }

    reports
}

fn detect_bool_judgement(function: &FunctionInfo) -> bool {
    let local_types = &function.local_types;
    for (offset, instr) in function.code.iter().enumerate() {
        match instr {
            SLBytecode::Call(_, _, Operation::Eq, srcs, _)
            | SLBytecode::Call(_, _, Operation::Neq, srcs, _) => {
                if srcs.len() != 2 {
                    continue;
                }
                let left = match get_def_bytecode(function, srcs[0], offset) {
                    Some(code) => code,
                    None => continue,
                };
                let right = match get_def_bytecode(function, srcs[1], offset) {
                    Some(code) => code,
                    None => continue,
                };

                if (is_ld_bool(left) && ret_is_bool(right, local_types))
                    || (is_ld_bool(right) && ret_is_bool(left, local_types))
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn is_ld_bool(bytecode: &SLBytecode) -> bool {
    matches!(
        bytecode,
        SLBytecode::Load(_, _, Constant::Bool(true))
            | SLBytecode::Load(_, _, Constant::Bool(false))
    )
}

fn ret_is_bool(bytecode: &SLBytecode, local_types: &[Type]) -> bool {
    match bytecode {
        SLBytecode::Call(_, dsts, _, _, _)
            if dsts.first().and_then(|idx| local_types.get(*idx))
                == Some(&Type::Primitive(PrimitiveType::Bool)) =>
        {
            true
        }
        SLBytecode::Assign(_, dst, _, _)
            if local_types.get(*dst) == Some(&Type::Primitive(PrimitiveType::Bool)) =>
        {
            true
        }
        SLBytecode::Load(_, dst, _)
            if local_types.get(*dst) == Some(&Type::Primitive(PrimitiveType::Bool)) =>
        {
            true
        }
        _ => false,
    }
}
