use move_binary_format::{
    CompiledModule,
    file_format::{Bytecode, FunctionHandle},
};
use serde::{Deserialize, Serialize};

use crate::abi::{
    MoveAbiSignatureToken, MoveFunctionAbi, MoveFunctionVisibility, MoveModuleAbi, MoveModuleId,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MoveFunctionCall {
    pub module: MoveModuleId,
    pub abi: MoveFunctionAbi,
    pub tys: Vec<MoveAbiSignatureToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveModuleBytecodeAnalysis {
    pub abi: MoveModuleAbi,
    pub calls: Vec<(MoveFunctionAbi, Vec<MoveFunctionCall>)>,
}

impl MoveModuleBytecodeAnalysis {
    fn handle_fcall(fcall: &FunctionHandle, module: &CompiledModule) -> MoveFunctionAbi {
        let fcall_function = module.identifier_at(fcall.name);
        let fcall_module = module.module_handle_at(fcall.module);
        let fcall_module_name = module.identifier_at(fcall_module.name);
        let fcall_address = module.address_identifier_at(fcall_module.address);

        if fcall_module_name == module.name() && fcall_address == module.address() {
            if let Some((_, fdef)) = module.find_function_def_by_name(fcall_function.as_str()) {
                MoveFunctionAbi::from_module_def(fdef, module)
            } else {
                log::warn!(
                    "Internal fcall {}:{}:{} not found",
                    fcall_address,
                    fcall_module_name,
                    fcall_function
                );
                MoveFunctionAbi::from_module_function_handle_visibility(
                    fcall,
                    module,
                    MoveFunctionVisibility::Public,
                )
            }
        } else {
            // Maybe friend visibility?!
            MoveFunctionAbi::from_module_function_handle_visibility(
                fcall,
                module,
                MoveFunctionVisibility::Public,
            )
        }
    }
    pub fn from_sui_module(module: &CompiledModule) -> Self {
        let abi = MoveModuleAbi::from_sui_module(module);

        let mut calls = vec![];
        for func in module.function_defs() {
            let caller_func = MoveFunctionAbi::from_module_def(func, module);
            let mut caller_calls = vec![];
            if let Some(code) = &func.code {
                for bytecode in &code.code {
                    match bytecode {
                        Bytecode::Call(call) => {
                            let fcall = module.function_handle_at(*call);
                            let abi = Self::handle_fcall(fcall, module);
                            let target_module = module.module_handle_at(fcall.module);
                            let target_module_id =
                                MoveModuleId::from_module_handle(target_module, module);
                            caller_calls.push(MoveFunctionCall {
                                module: target_module_id,
                                abi,
                                tys: vec![],
                            });
                        }
                        Bytecode::CallGeneric(inst) => {
                            let inst = module.function_instantiation_at(*inst);
                            let fcall = module.function_handle_at(inst.handle);
                            let abi = Self::handle_fcall(fcall, module);
                            let tys = module.signature_at(inst.type_parameters);
                            let tys = tys
                                .0
                                .iter()
                                .map(|v| {
                                    MoveAbiSignatureToken::from_sui_token_module(
                                        v,
                                        &caller_func.type_parameters,
                                        module,
                                    )
                                })
                                .collect();
                            let target_module = module.module_handle_at(fcall.module);
                            let target_module_id =
                                MoveModuleId::from_module_handle(target_module, module);
                            caller_calls.push(MoveFunctionCall {
                                module: target_module_id,
                                abi,
                                tys,
                            });
                        }
                        _ => {}
                    }
                }
            } else {
                log::debug!("Function {:?} has no code", &func);
            }
            calls.push((caller_func, caller_calls));
        }
        Self { abi, calls }
    }
}
