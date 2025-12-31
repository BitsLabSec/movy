use std::collections::{BTreeMap, BTreeSet};

use log::{debug, warn};
use movy_types::{
    abi::MoveFunctionAbi,
    input::{
        FunctionIdent, MoveCall, MoveSequence, MoveSequenceCall, MoveTypeTag, SequenceArgument,
    },
};

use crate::{
    meta::{FunctionHook, FuzzMetadata, HasFuzzMetadata},
    mutators::sequence::{
        append::append_function,
        post::{
            process_balance, process_key_store, remove_process_balance, remove_process_key_store,
        },
        remap::{remap_command, remap_command_with_map},
    },
    state::HasFuzzEnv,
};
use libafl::state::HasRand;

pub fn context_idents(meta: &FuzzMetadata) -> Option<(FunctionIdent, FunctionIdent)> {
    let get_one = |name: &str| {
        meta.function_name_to_idents.get(name).and_then(|v| {
            v.iter()
                .find(|ident| ident.0.module_name == "context")
                .cloned()
                .or_else(|| v.first().cloned())
        })
    };
    let create = get_one("create_context")?;
    let destroy = get_one("destroy_context")?;
    Some((create, destroy))
}

fn hook_idents(meta: &FuzzMetadata) -> BTreeSet<FunctionIdent> {
    let mut hooks = BTreeSet::new();
    hooks.extend(meta.sequence_hooks.pre_hooks.iter().cloned());
    hooks.extend(meta.sequence_hooks.post_hooks.iter().cloned());
    for hook in meta.function_hooks.values() {
        hooks.extend(hook.pre_hooks.iter().cloned());
        hooks.extend(hook.post_hooks.iter().cloned());
    }
    if let Some((create, destroy)) = context_idents(meta) {
        hooks.insert(create);
        hooks.insert(destroy);
    }
    hooks
}

pub fn strip_hooks_only(ptb: &MoveSequence, meta: &FuzzMetadata) -> MoveSequence {
    let hook_idents = hook_idents(meta);
    let mut mapping: Vec<Option<u16>> = Vec::with_capacity(ptb.commands.len());
    let mut kept: Vec<MoveSequenceCall> = Vec::new();
    for cmd in ptb.commands.iter() {
        let is_hook = matches!(cmd, MoveSequenceCall::Call(mc) if hook_idents.contains(&FunctionIdent::new(&mc.module_id, &mc.module_name, &mc.function)));
        if is_hook {
            mapping.push(None);
            continue;
        }
        mapping.push(Some(kept.len() as u16));
        kept.push(cmd.clone());
    }
    let remapped = kept
        .into_iter()
        .filter_map(|cmd| remap_command_with_map(&cmd, &mapping))
        .collect();
    MoveSequence {
        inputs: ptb.inputs.clone(),
        commands: remapped,
    }
}

pub fn strip_generated(ptb: &MoveSequence, meta: &FuzzMetadata) -> MoveSequence {
    let mut base = strip_hooks_only(ptb, meta);
    remove_process_key_store(&mut base);
    remove_process_balance(&mut base);
    base
}

fn append_hook_call<S>(
    state: &mut S,
    ptb: &mut MoveSequence,
    hook_ident: &FunctionIdent,
    target: Option<(&MoveCall, &MoveFunctionAbi, Option<u16>)>,
    ctx_arg: Option<SequenceArgument>,
) where
    S: HasRand + HasFuzzMetadata + HasFuzzEnv,
{
    let meta = state.fuzz_state();
    let hook_abi = match meta.get_function(
        &hook_ident.0.module_address,
        &hook_ident.0.module_name,
        &hook_ident.1,
    ) {
        Some(f) => f.clone(),
        None => {
            warn!("Unknown hook function {}", hook_ident);
            return;
        }
    };

    let mut fixed_args: BTreeMap<u16, (SequenceArgument, MoveTypeTag)> = BTreeMap::new();
    let mut fixed_ty_args: BTreeMap<u16, MoveTypeTag> = BTreeMap::new();
    let param_offset = if ctx_arg.is_some() { 1 } else { 0 };
    if let Some(ctx) = ctx_arg {
        if let Some(param_ty) = hook_abi
            .parameters
            .get(0)
            .and_then(|p| p.subst(&BTreeMap::new()))
        {
            fixed_args.insert(0, (ctx, param_ty));
        }
    }
    if let Some((target_call, target_abi, _maybe_idx)) = target {
        // Hook must mirror the target function signature (plus optional context).
        if hook_abi.type_parameters.len() != target_call.type_arguments.len() {
            warn!(
                "skip hook {} due to mismatched type parameters: hook {} vs target {}",
                hook_ident,
                hook_abi.type_parameters.len(),
                target_call.type_arguments.len()
            );
            return;
        }
        for (i, ty) in target_call.type_arguments.iter().enumerate() {
            fixed_ty_args.insert(i as u16, ty.clone());
        }

        if hook_abi.parameters.len() != param_offset + target_abi.parameters.len() {
            warn!(
                "skip hook {} due to mismatched params: hook {} vs target {} (+ctx {})",
                hook_ident,
                hook_abi.parameters.len(),
                target_abi.parameters.len(),
                param_offset
            );
            return;
        }

        let ty_args_map = target_call
            .type_arguments
            .iter()
            .enumerate()
            .map(|(i, ty)| (i as u16, ty.clone()))
            .collect::<BTreeMap<_, _>>();
        let hook_ty_args_map = fixed_ty_args.clone();
        for (i, (hook_param, target_param)) in hook_abi
            .parameters
            .iter()
            .skip(param_offset)
            .zip(target_abi.parameters.iter())
            .enumerate()
        {
            let Some(hook_ty) = hook_param.subst(&hook_ty_args_map) else {
                warn!(
                    "skip hook {} due to unsubstitutable hook param {}",
                    hook_ident, i
                );
                return;
            };
            let Some(target_ty) = target_param.subst(&ty_args_map) else {
                warn!(
                    "skip hook {} due to unsubstitutable target param {}",
                    hook_ident, i
                );
                return;
            };
            if hook_ty != target_ty {
                warn!(
                    "skip hook {} due to param type mismatch at {}: hook {} vs target {}",
                    hook_ident, i, hook_ty, target_ty
                );
                return;
            }
            let arg = target_call
                .arguments
                .get(i)
                .cloned()
                .unwrap_or_else(|| SequenceArgument::Input(0));
            fixed_args.insert((param_offset + i) as u16, (arg, hook_ty));
        }
    }

    let used_arguments = fixed_args.values().map(|(arg, _)| *arg).collect();
    if append_function(
        state,
        ptb,
        hook_ident,
        fixed_args,
        fixed_ty_args,
        &used_arguments,
        true,
        0,
    )
    .is_none()
    {
        warn!("skip hook {} due to arg construction failure", hook_ident);
    }
}

fn resolve_destroy_param_ty<S>(
    state: &S,
    context_idents: &Option<(FunctionIdent, FunctionIdent)>,
) -> Option<MoveTypeTag>
where
    S: HasFuzzMetadata,
{
    context_idents.as_ref().and_then(|(_, destroy_ctx)| {
        state
            .fuzz_state()
            .get_function(
                &destroy_ctx.0.module_address,
                &destroy_ctx.0.module_name,
                &destroy_ctx.1,
            )
            .and_then(|abi| {
                abi.parameters
                    .first()
                    .and_then(|p| p.subst(&BTreeMap::new()))
            })
    })
}

fn append_context_creation<S>(
    state: &mut S,
    ptb: &mut MoveSequence,
    context_idents: &Option<(FunctionIdent, FunctionIdent)>,
    scope: &str,
) -> Option<SequenceArgument>
where
    S: HasRand + HasFuzzMetadata + HasFuzzEnv,
{
    let Some((create_ctx, _)) = context_idents.as_ref() else {
        debug!("[{scope}] No context creation function available");
        return None;
    };

    debug!("[{scope}] Appending context creation hook: {}", create_ctx);
    if let Some((_, rets)) = append_function(
        state,
        ptb,
        create_ctx,
        BTreeMap::new(),
        BTreeMap::new(),
        &vec![],
        true,
        0,
    ) {
        let ctx = rets.first().cloned();
        debug!("[{scope}] Context created with return value: {:?}", ctx);
        ctx
    } else {
        warn!(
            "[{scope}] Failed to append context creation function {}",
            create_ctx
        );
        None
    }
}

fn append_context_destruction<S>(
    state: &mut S,
    ptb: &mut MoveSequence,
    context_idents: &Option<(FunctionIdent, FunctionIdent)>,
    destroy_param_ty: &Option<MoveTypeTag>,
    ctx_arg: Option<SequenceArgument>,
    scope: &str,
) where
    S: HasRand + HasFuzzMetadata + HasFuzzEnv,
{
    let (Some(ctx_arg), Some((_, destroy_ctx)), Some(param_ty)) =
        (ctx_arg, context_idents.as_ref(), destroy_param_ty)
    else {
        if ctx_arg.is_none() {
            debug!("[{scope}] No context argument to destroy");
        } else {
            warn!("[{scope}] Unknown destroy context function or param type");
        }
        return;
    };

    debug!(
        "[{scope}] Appending context destruction hook: {} with arg {}",
        destroy_ctx, ctx_arg
    );
    let mut fixed_args = BTreeMap::new();
    fixed_args.insert(0u16, (ctx_arg, param_ty.clone()));
    let _ = append_function(
        state,
        ptb,
        destroy_ctx,
        fixed_args,
        BTreeMap::new(),
        &vec![],
        true,
        0,
    );
}

fn append_sequence_hooks_with_ctx<S>(
    state: &mut S,
    hooks: &[FunctionIdent],
    ptb: &mut MoveSequence,
    ctx: Option<SequenceArgument>,
    phase: &str,
) where
    S: HasRand + HasFuzzMetadata + HasFuzzEnv,
{
    for hook in hooks.iter() {
        debug!("[{phase}] Appending sequence hook {}", hook);
        append_hook_call(state, ptb, hook, None, ctx);
    }
}

fn append_call_without_hooks(
    ptb: &mut MoveSequence,
    index_map: &mut Vec<u16>,
    remapped_call: MoveCall,
    original_idx: usize,
) {
    let new_idx = ptb.commands.len() as u16;
    debug!("[cmd {original_idx}] No hooks found, appending call at new idx {new_idx}");
    ptb.commands.push(MoveSequenceCall::Call(remapped_call));
    index_map.push(new_idx);
}

fn append_non_call_command(
    ptb: &mut MoveSequence,
    index_map: &mut Vec<u16>,
    cmd: &MoveSequenceCall,
    original_idx: usize,
) {
    let new_idx = ptb.commands.len() as u16;
    debug!("[cmd {original_idx}] Appending non-call command at new idx {new_idx}");
    ptb.commands.push(remap_command(cmd, index_map));
    index_map.push(new_idx);
}

fn handle_function_call_hooks<S>(
    state: &mut S,
    movecall: &MoveCall,
    remapped_call: MoveCall,
    ptb: &mut MoveSequence,
    index_map: &mut Vec<u16>,
    function_hooks: &BTreeMap<FunctionIdent, FunctionHook>,
    context_idents: &Option<(FunctionIdent, FunctionIdent)>,
    destroy_param_ty: &Option<MoveTypeTag>,
    original_idx: usize,
) -> bool
where
    S: HasRand + HasFuzzMetadata + HasFuzzEnv,
{
    let func_ident = FunctionIdent::new(
        &movecall.module_id,
        &movecall.module_name,
        &movecall.function,
    );
    let target_abi = state
        .fuzz_state()
        .get_function(
            &movecall.module_id,
            &movecall.module_name,
            &movecall.function,
        )
        .cloned();

    let Some(target_abi) = target_abi else {
        debug!(
            "[cmd {original_idx}] Target ABI not found for {}",
            func_ident
        );
        return false;
    };
    let Some(hooks) = function_hooks.get(&func_ident) else {
        debug!(
            "[cmd {original_idx}] No function-level hooks for {}",
            func_ident
        );
        return false;
    };

    debug!(
        "[cmd {original_idx}] Applying hooks for target {}: pre [{}], post [{}]",
        func_ident,
        hooks
            .pre_hooks
            .iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        hooks
            .post_hooks
            .iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let hook_ctx = append_context_creation(state, ptb, context_idents, "function");

    for hook in hooks.pre_hooks.iter() {
        debug!("[cmd {original_idx}] Appending pre-hook {}", hook);
        append_hook_call(
            state,
            ptb,
            hook,
            Some((&remapped_call, &target_abi, None)),
            hook_ctx,
        );
    }

    let main_idx = ptb.commands.len() as u16;
    debug!("[cmd {original_idx}] Appending main call {func_ident} at new idx {main_idx}");
    ptb.commands
        .push(MoveSequenceCall::Call(remapped_call.clone()));
    index_map.push(main_idx);

    for hook in hooks.post_hooks.iter() {
        debug!("[cmd {original_idx}] Appending post-hook {}", hook);
        append_hook_call(
            state,
            ptb,
            hook,
            Some((&remapped_call, &target_abi, Some(main_idx))),
            hook_ctx,
        );
    }

    append_context_destruction(
        state,
        ptb,
        context_idents,
        destroy_param_ty,
        hook_ctx,
        "function",
    );

    true
}

pub fn apply_hooks<S>(state: &mut S, base: &MoveSequence) -> MoveSequence
where
    S: HasRand + HasFuzzMetadata + HasFuzzEnv,
{
    debug!("Applying hooks to sequence {}", base,);
    let function_hooks = state.fuzz_state().function_hooks.clone();
    let sequence_hooks = state.fuzz_state().sequence_hooks.clone();
    debug!(
        "Detected movy test sequence hooks: pre [{}], post [{}]",
        sequence_hooks
            .pre_hooks
            .iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>()
            .join(", "),
        sequence_hooks
            .post_hooks
            .iter()
            .map(|h| h.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if function_hooks.is_empty() {
        debug!("No movy test function-level hooks detected");
    } else {
        for (target, hooks) in function_hooks.iter() {
            debug!(
                "Detected movy test hooks for target {}: pre [{}], post [{}]",
                target,
                hooks
                    .pre_hooks
                    .iter()
                    .map(|h| h.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                hooks
                    .post_hooks
                    .iter()
                    .map(|h| h.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    let context_idents = context_idents(state.fuzz_state());
    let destroy_param_ty = resolve_destroy_param_ty(state, &context_idents);

    let mut ptb = MoveSequence {
        inputs: base.inputs.clone(),
        commands: vec![],
    };
    let mut index_map: Vec<u16> = Vec::with_capacity(base.commands.len());

    let global_ctx: Option<SequenceArgument> =
        append_context_creation(state, &mut ptb, &context_idents, "global");

    append_sequence_hooks_with_ctx(
        state,
        &sequence_hooks.pre_hooks,
        &mut ptb,
        global_ctx,
        "seq-pre",
    );

    for (original_idx, cmd) in base.commands.iter().enumerate() {
        match cmd {
            MoveSequenceCall::Call(movecall) => {
                let remapped_call = match remap_command(cmd, &index_map) {
                    MoveSequenceCall::Call(mc) => mc,
                    _ => unreachable!("expected MoveCall"),
                };
                if handle_function_call_hooks(
                    state,
                    movecall,
                    remapped_call.clone(),
                    &mut ptb,
                    &mut index_map,
                    &function_hooks,
                    &context_idents,
                    &destroy_param_ty,
                    original_idx,
                ) {
                    continue;
                }
                append_call_without_hooks(&mut ptb, &mut index_map, remapped_call, original_idx);
            }
            _ => {
                append_non_call_command(&mut ptb, &mut index_map, cmd, original_idx);
            }
        }
    }

    append_sequence_hooks_with_ctx(
        state,
        &sequence_hooks.post_hooks,
        &mut ptb,
        global_ctx,
        "seq-post",
    );

    append_context_destruction(
        state,
        &mut ptb,
        &context_idents,
        &destroy_param_ty,
        global_ctx,
        "global",
    );

    process_balance(&mut ptb, state);
    process_key_store(&mut ptb, state);

    ptb
}
