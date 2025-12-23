use move_model::ty::{PrimitiveType, Type};
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
            if detect_unnecessary_type_conversion(function) {
                reports.push(OracleFinding {
                    oracle: "StaticTypeConversion".to_string(),
                    severity: Severity::Minor,
                    extra: json!({
                        "module": module.qualified_module_name(),
                        "function": function.name.clone(),
                        "message": "Unnecessary type conversion"
                    }),
                });
            }
        }
    }

    reports
}

fn detect_unnecessary_type_conversion(function: &FunctionInfo) -> bool {
    for instr in function.code.iter() {
        match instr {
            SLBytecode::Call(_, _, Operation::CastU8, srcs, _) => {
                if function.local_types[srcs[0]] == Type::Primitive(PrimitiveType::U8) {
                    return true;
                }
            }
            SLBytecode::Call(_, _, Operation::CastU16, srcs, _) => {
                if function.local_types[srcs[0]] == Type::Primitive(PrimitiveType::U16) {
                    return true;
                }
            }
            SLBytecode::Call(_, _, Operation::CastU32, srcs, _) => {
                if function.local_types[srcs[0]] == Type::Primitive(PrimitiveType::U32) {
                    return true;
                }
            }
            SLBytecode::Call(_, _, Operation::CastU64, srcs, _) => {
                if function.local_types[srcs[0]] == Type::Primitive(PrimitiveType::U64) {
                    return true;
                }
            }
            SLBytecode::Call(_, _, Operation::CastU128, srcs, _) => {
                if function.local_types[srcs[0]] == Type::Primitive(PrimitiveType::U128) {
                    return true;
                }
            }
            SLBytecode::Call(_, _, Operation::CastU256, srcs, _) => {
                if function.local_types[srcs[0]] == Type::Primitive(PrimitiveType::U256) {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}
