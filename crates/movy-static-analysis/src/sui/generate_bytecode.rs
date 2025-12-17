use anyhow::anyhow;
use movy_types::error::MovyError;

use log::{debug, trace, warn};
use move_binary_format::CompiledModule;
use move_model::{
    ast::ModuleName,
    model::{DatatypeId, FunId, FunctionVisibility, GlobalEnv, Loc, ModuleId},
    run_bytecode_model_builder,
    symbol::SymbolPool,
    ty::Type,
};
use move_stackless_bytecode::{
    function_target::FunctionData,
    stackless_bytecode::{AbortAction, AttrId, Bytecode},
    stackless_bytecode_generator::StacklessBytecodeGenerator as MoveStacklessGenerator,
    stackless_control_flow_graph::StacklessControlFlowGraph,
};
use std::collections::{BTreeMap, BTreeSet};

/// Stackless bytecode for a single function inside a compiled module.
pub struct FunctionInfo {
    pub module_id: ModuleId,
    pub fun_id: FunId,
    pub idx: usize,
    pub name: String,
    pub visibility: FunctionVisibility,
    pub is_entry: bool,
    pub param_count: usize,
    pub args_count: usize,
    pub code: Vec<Bytecode>,
    pub local_types: Vec<Type>,
    pub return_types: Vec<Type>,
    pub acquires: Vec<DatatypeId>,
    pub location_table: BTreeMap<AttrId, Loc>,
    pub loop_invariants: BTreeSet<AttrId>,
    pub cfg: Option<StacklessControlFlowGraph>,
    pub def_attrid: Vec<Vec<usize>>,
    pub use_attrid: Vec<Vec<usize>>,
}

/// Stackless bytecode for an entire module.
pub struct ModuleBytecode {
    pub id: ModuleId,
    pub name: ModuleName,
    pub functions: Vec<FunctionInfo>,
}

pub struct GlobalInfo {
    pub symbol_pool: SymbolPool,
    pub modules: Vec<ModuleBytecode>,
}

/// Build stackless bytecode for a collection of compiled modules.
///
/// The iterator of modules must be ordered topologically by dependency (callees before callers).
/// Dependencies referenced from the target modules need to be present so that signature
/// globalization can succeed.
pub fn generate_stackless_bytecode<'a>(
    modules: impl IntoIterator<Item = &'a CompiledModule>,
) -> Result<(Vec<ModuleBytecode>, GlobalEnv), MovyError> {
    let modules: Vec<&CompiledModule> = modules.into_iter().collect();
    if modules.is_empty() {
        return Err(MovyError::Any(anyhow!(
            "no modules provided to stackless generator"
        )));
    }

    let env = run_bytecode_model_builder(modules.iter().copied())?;
    debug!("symbol pool: {:?}", env.symbol_pool());
    let mut output = Vec::new();

    trace!(
        "Generating stackless bytecode for {} modules",
        env.get_modules().count()
    );
    for module_env in env.get_modules() {
        let mut functions = Vec::new();
        trace!(
            "Generating stackless bytecode for module {}",
            module_env.get_full_name_str()
        );
        for func_env in module_env.get_functions() {
            if func_env.is_native() {
                continue;
            }
            trace!(
                "Generated stackless bytecode for {}::{}::{}, function {:?}, local count: {}",
                module_env.get_name().addr(),
                env.symbol_pool()
                    .string(module_env.get_name().name())
                    .as_str(),
                func_env.get_name_str(),
                func_env.get_id(),
                func_env.get_local_count()
            );
            if !func_env.get_jump_tables().is_empty() {
                warn!("currently jump tables are ignored");
                continue;
            }

            let FunctionData {
                code,
                local_types,
                return_types,
                acquires_global_resources,
                locations,
                loop_invariants,
                ..
            } = MoveStacklessGenerator::new(&func_env).generate_function();

            let args_count = local_types.len();
            let (def_attrid, use_attrid) = compute_def_use(args_count, &code);
            let cfg = if code.is_empty() {
                None
            } else {
                Some(StacklessControlFlowGraph::new_forward(&code))
            };

            functions.push(FunctionInfo {
                module_id: module_env.get_id(),
                fun_id: func_env.get_id(),
                idx: func_env.get_def_idx().0 as usize,
                name: func_env.get_name_str(),
                visibility: func_env.visibility(),
                is_entry: func_env.is_entry(),
                param_count: func_env.get_parameters().len(),
                args_count,
                code,
                local_types,
                return_types,
                acquires: acquires_global_resources,
                location_table: locations,
                loop_invariants,
                cfg,
                def_attrid,
                use_attrid,
            });
        }

        output.push(ModuleBytecode {
            id: module_env.get_id(),
            name: module_env.get_name().clone(),
            functions,
        });
    }

    Ok((output, env))
}

/// Convenience helper which returns the stackless bytecode for a single module. The `module`
/// should appear after all of its dependencies in the iterator.
pub fn generate_stackless_bytecode_for_module<'a>(
    dependencies: impl IntoIterator<Item = &'a CompiledModule>,
    module: &'a CompiledModule,
) -> Result<(ModuleBytecode, GlobalEnv), MovyError> {
    let mut modules: Vec<&CompiledModule> = dependencies.into_iter().collect();
    modules.push(module);

    let mut all = generate_stackless_bytecode(modules)?;
    all.0
        .pop()
        .map(|m| (m, all.1))
        .ok_or_else(|| MovyError::Any(anyhow!("stackless generator produced no modules")))
}

fn compute_def_use(locals_len: usize, code: &[Bytecode]) -> (Vec<Vec<usize>>, Vec<Vec<usize>>) {
    let mut defs = vec![Vec::new(); locals_len];
    let mut uses = vec![Vec::new(); locals_len];

    for (offset, instr) in code.iter().enumerate() {
        match instr {
            Bytecode::Assign(_, dst, src, _) => {
                record(&mut defs, *dst, offset);
                record(&mut uses, *src, offset);
            }
            Bytecode::Call(_, dsts, _, srcs, abort_action) => {
                for dst in dsts {
                    record(&mut defs, *dst, offset);
                }
                for src in srcs {
                    record(&mut uses, *src, offset);
                }
                if let Some(AbortAction(_, temp)) = abort_action {
                    record(&mut defs, *temp, offset);
                }
            }
            Bytecode::Ret(_, srcs) => {
                for src in srcs {
                    record(&mut uses, *src, offset);
                }
            }
            Bytecode::Load(_, dst, _) => {
                record(&mut defs, *dst, offset);
            }
            Bytecode::Branch(_, _, _, src) | Bytecode::Abort(_, src) => {
                record(&mut uses, *src, offset);
            }
            Bytecode::VariantSwitch(_, discr, _) => {
                record(&mut uses, *discr, offset);
            }
            Bytecode::Jump(..) | Bytecode::Label(..) | Bytecode::Nop(..) => {}
        }
    }

    (defs, uses)
}

fn record(slots: &mut [Vec<usize>], idx: usize, offset: usize) {
    if let Some(list) = slots.get_mut(idx) {
        list.push(offset);
    }
}
