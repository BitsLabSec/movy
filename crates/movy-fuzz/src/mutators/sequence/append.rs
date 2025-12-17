use std::collections::{BTreeMap, BTreeSet};

use libafl::state::HasRand;
use libafl_bolts::rands::Rand;
use log::debug;
use movy_types::{
    abi::MoveAbiSignatureToken,
    input::{
        FunctionIdent, InputArgument, MoveAddress, MoveCall, MoveSequence, MoveSequenceCall,
        MoveTypeTag, SequenceArgument,
    },
};

use crate::{
    meta::HasFuzzMetadata,
    mutators::{
        mutation_utils::MutableValue,
        object_data::{ConstructResult, ObjectData, gen_type_tag_by_abilities, try_construct_args},
    },
    state::HasFuzzEnv,
};

pub fn weighted_sample<'a, T>(items: &'a [T], weights: &[u64], state: &mut impl HasRand) -> &'a T {
    assert_eq!(
        items.len(),
        weights.len(),
        "Items and weights must have the same length"
    );
    assert!(!weights.is_empty(), "Weights cannot be empty");

    let total_weight: u64 = weights.iter().copied().sum();

    if total_weight == 0 {
        panic!("Total weight cannot be zero");
    }

    let random_number = state.rand_mut().below_or_zero(total_weight as usize) + 1;

    let mut cumulative_weight = 0;
    for (i, &weight) in weights.iter().enumerate() {
        cumulative_weight += weight;

        if random_number <= cumulative_weight as usize {
            return &items[i];
        }
    }

    unreachable!();
}

pub fn append_function<S>(
    state: &mut S,
    ptb: &mut MoveSequence,
    function_ident: &FunctionIdent,
    fixed_args: BTreeMap<u16, (SequenceArgument, MoveTypeTag)>,
    fixed_ty_args: BTreeMap<u16, MoveTypeTag>,
    used_arguments: &Vec<SequenceArgument>,
    disable_split: bool,
    recursion_depth: usize,
) -> Option<(Vec<SequenceArgument>, Vec<SequenceArgument>)>
// Returns the arguments and returns
where
    S: HasRand + HasFuzzMetadata + HasFuzzEnv,
{
    if recursion_depth > 10 {
        debug!(
            "Recursion depth exceeded for function: {:?}, skipping",
            function_ident
        );
        return None;
    }
    let object_data = { ObjectData::from_ptb_and_remove_used(ptb, state, used_arguments) };
    let (addr, mname, fname) = (
        function_ident.0.module_address,
        &function_ident.0.module_name,
        &function_ident.1,
    );
    let mut cmd = MoveSequenceCall::Call(MoveCall {
        module_id: addr,
        module_name: mname.clone(),
        function: fname.clone(),
        type_arguments: vec![],
        arguments: vec![],
    });
    let MoveSequenceCall::Call(movecall) = &mut cmd else {
        panic!("Expected MoveCall command");
    };
    debug!("Adding function: {:?}", function_ident);
    let function = state
        .fuzz_state()
        .get_function(&addr, mname, fname)
        .unwrap_or_else(|| panic!("Function not found: {}::{}::{}", addr, mname, fname))
        .clone();
    let mut struct_params = function
        .parameters
        .clone()
        .into_iter()
        .enumerate()
        .filter_map(|(i, p)| {
            if fixed_args.contains_key(&(i as u16)) {
                return None; // Skip fixed arguments
            }
            if p.needs_sample() {
                Some(p)
            } else {
                None // Skip parameters that don't need sampling
            }
        })
        .collect::<Vec<_>>();
    let mut fixed_ty_args = fixed_ty_args;
    for (i, (_, ty_tag)) in fixed_args.iter() {
        if fixed_ty_args.contains_key(i) && fixed_ty_args[i] != *ty_tag {
            panic!("Conflicting type arguments for index {}", i);
        }
        fixed_ty_args.insert(*i, ty_tag.clone());
    }
    let initial_ptb_input_len = ptb.inputs.len();
    let mut inputs = match try_construct_args(
        initial_ptb_input_len,
        &struct_params,
        &function.type_parameters,
        &fixed_ty_args,
        &object_data,
        state,
    ) {
        ConstructResult::Ok(mut args, ty_args, inputs) => {
            movecall.type_arguments = ty_args;

            let mut inputs = inputs;
            for (i, param) in function.parameters.iter().enumerate() {
                if let Some((arg, _)) = fixed_args.get(&(i as u16)) {
                    movecall.arguments.push(*arg);
                    continue; // Use fixed argument if available
                }
                if param.needs_sample() {
                    movecall.arguments.push(args.remove(0));
                } else {
                    if param.is_tx_context() {
                        // Skip tx context parameters
                        continue;
                    }
                    debug!("Generating initial value for parameter {}: {:?}", i, param);
                    let init_value = param.gen_input_arg().unwrap_or_else(|| {
                        panic!(
                            "Failed to generate initial value for parameter {}: {:?}",
                            i, param
                        )
                    });
                    let mut init_value = MutableValue::new(init_value);
                    init_value.mutate(state, &BTreeSet::new(), false);
                    let init_value = init_value.value;
                    inputs.push(init_value);
                    movecall.arguments.push(SequenceArgument::Input(
                        (ptb.inputs.len() + inputs.len() - 1) as u16,
                    ));
                }
            }
            inputs
        }
        ConstructResult::PartialFound(mut partial_args, ty_args, inputs) => {
            debug!(
                "Partial arguments found for function: {}::{}::{}, got {:?}",
                addr,
                mname,
                fname,
                partial_args
                    .iter()
                    .enumerate()
                    .filter_map(|(i, arg)| arg.and_then(|_| Some(i)))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                partial_args.len(),
                struct_params.len(),
                "Partial arguments length mismatch"
            );
            let mut inputs = inputs;
            let mut type_arguments: BTreeMap<u16, MoveTypeTag> = BTreeMap::new();
            let mut new_used_arguments = partial_args
                .iter()
                .zip(struct_params.iter())
                .filter_map(|(arg, param)| {
                    if let Some(arg) = arg {
                        if matches!(
                            param,
                            MoveAbiSignatureToken::Reference(_)
                                | MoveAbiSignatureToken::MutableReference(_)
                        ) {
                            return None;
                        }
                        Some(*arg)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            new_used_arguments.extend(used_arguments.iter().cloned());
            debug!("Used arguments for producing graph: {:?}", used_arguments);
            for (i, param) in function.parameters.iter().enumerate() {
                if let Some((arg, _)) = fixed_args.get(&(i as u16)) {
                    movecall.arguments.push(*arg);
                    continue; // Use fixed argument if available
                }
                if param.needs_sample() {
                    if let Some(arg) = partial_args.remove(0) {
                        struct_params.remove(0);
                        movecall.arguments.push(arg);
                        continue; // Use sampled argument
                    }
                    let arg_type = struct_params.remove(0).partial_subst(&ty_args);

                    let funcs = state.fuzz_state().type_graph.find_producers(&arg_type);
                    // except itself
                    let funcs = funcs
                        .iter()
                        .filter(|(m, f)| {
                            !(m.module_address == addr
                                && &m.module_name == mname
                                && &f.name == fname)
                        })
                        .collect::<Vec<_>>();
                    if funcs.is_empty() {
                        debug!(
                            "No producing functions found for type {:?} in {:?}::{:?}",
                            arg_type, addr, mname
                        );
                        return None;
                    }

                    let pre_func = funcs[state.rand_mut().below_or_zero(funcs.len())].clone();
                    debug!(
                        "Using producing function {:?} for argument type {:?}",
                        pre_func, arg_type
                    );
                    let Some((arg_ty_arg, mut producing_ty_arg, mapping, ret_idx)) = pre_func
                        .1
                        .return_paramters
                        .iter()
                        .enumerate()
                        .filter_map(|(i, ret_ty)| {
                            arg_type.partial_extract_ty_args(ret_ty).map(
                                |(arg_ty_arg, producing_ty_arg, mapping)| {
                                    (arg_ty_arg, producing_ty_arg, mapping, i as u16)
                                },
                            )
                        })
                        .next()
                    else {
                        debug!(
                            "No suitable return type found for producing function {:?} for argument type {:?}",
                            pre_func, arg_type
                        );
                        return None;
                    };
                    for (self_, other) in mapping.iter() {
                        if let Some(ty_arg) = type_arguments.get(self_) {
                            if let Some(other_ty_arg) = producing_ty_arg.get(other) {
                                if *ty_arg != other_ty_arg.clone() {
                                    debug!(
                                        "Type argument conflict for producing function {:?}: {:?} vs {:?}",
                                        pre_func, ty_arg, other_ty_arg
                                    );
                                    return None;
                                }
                            } else {
                                producing_ty_arg.insert(*other, ty_arg.clone());
                            }
                        }
                    }
                    let pre_func_ident = FunctionIdent::new(
                        &pre_func.0.module_address,
                        &pre_func.0.module_name,
                        &pre_func.1.name,
                    );
                    if let Some((_, new_rets)) = append_function(
                        state,
                        ptb,
                        &pre_func_ident,
                        BTreeMap::new(),
                        producing_ty_arg,
                        &new_used_arguments,
                        false,
                        recursion_depth + 1,
                    ) {
                        let MoveSequenceCall::Call(new_movecall) = ptb.commands.last_mut().unwrap()
                        else {
                            panic!("Expected MoveCall command");
                        };
                        let mut ty_args: BTreeMap<u16, MoveTypeTag> =
                            ty_args.clone().into_iter().collect::<BTreeMap<_, _>>();
                        ty_args.extend(arg_ty_arg.iter().map(|(j, ty_arg)| (*j, ty_arg.clone())));
                        for m in mapping {
                            ty_args.insert(m.0, new_movecall.type_arguments[m.1 as usize].clone());
                        }

                        type_arguments.extend(ty_args);
                        movecall.arguments.push(new_rets[ret_idx as usize]);

                        if !(matches!(
                            param,
                            MoveAbiSignatureToken::Reference(_)
                                | MoveAbiSignatureToken::MutableReference(_)
                        )) {
                            new_used_arguments.push(new_rets[ret_idx as usize]);
                        }
                    } else {
                        debug!(
                            "Failed to append function for partial argument: {}",
                            pre_func_ident
                        );
                        return None;
                    }
                } else {
                    if param.is_tx_context() {
                        // Skip tx context parameters
                        continue;
                    }
                    debug!("Generating initial value for parameter {}: {:?}", i, param);
                    let init_value = param.gen_input_arg().unwrap();
                    let mut init_value = MutableValue::new(init_value);
                    init_value.mutate(state, &BTreeSet::new(), false);
                    let init_value = init_value.value;
                    inputs.push(init_value);
                    movecall.arguments.push(SequenceArgument::Input(
                        (initial_ptb_input_len + inputs.len() - 1) as u16,
                    ));
                }
            }
            movecall.type_arguments = function
                .type_parameters
                .iter()
                .enumerate()
                .map(|(j, ability)| {
                    type_arguments
                        .get(&(j as u16))
                        .cloned()
                        .unwrap_or(gen_type_tag_by_abilities(ability, state))
                })
                .collect();

            inputs
        }
        ConstructResult::Unsolvable => {
            debug!(
                "Failed to construct arguments for function: {}::{}::{}",
                addr, mname, fname
            );
            return None;
        }
    };

    // after appending sub functions, we need to adjust Input arguments offsets
    let mut remove_idxs = vec![];
    let mut fixed_idxs = vec![];
    for (i, (arg, param)) in movecall
        .arguments
        .iter_mut()
        .zip(function.parameters.iter())
        .enumerate()
    {
        if let SequenceArgument::Input(input_idx) = arg {
            if (*input_idx as usize) < initial_ptb_input_len {
                continue; // Skip inputs that are not newly added
            }
            let offset = *input_idx as usize - initial_ptb_input_len;
            let input = &inputs[offset];
            if matches!(input, InputArgument::Object(_, _)) && ptb.inputs.contains(input) {
                if matches!(
                    param,
                    MoveAbiSignatureToken::Reference(_)
                        | MoveAbiSignatureToken::MutableReference(_)
                ) {
                    *input_idx = ptb.inputs.iter().position(|x| x == input).unwrap() as u16;
                    fixed_idxs.push(i);
                    remove_idxs.push(offset);
                    continue;
                } else {
                    return None;
                }
            }
        }
    }
    remove_idxs.sort_unstable();
    remove_idxs.dedup();
    for remove_idx in remove_idxs.iter().rev() {
        inputs.remove(*remove_idx);
    }
    for (i, arg) in movecall.arguments.iter_mut().enumerate() {
        if fixed_idxs.contains(&i) {
            continue;
        }
        if let SequenceArgument::Input(input_idx) = arg
            && *input_idx as usize >= initial_ptb_input_len
        {
            let offset = *input_idx as usize - initial_ptb_input_len;
            let adjust = remove_idxs.iter().filter(|&&x| x < offset).count();
            *input_idx -= adjust as u16;
            *input_idx += (ptb.inputs.len() - initial_ptb_input_len) as u16
        }
    }

    if !disable_split {
        // Handle balance parameters
        for (arg, param) in movecall
            .arguments
            .iter_mut()
            .zip(function.parameters.iter())
        {
            if param.is_balance() || param.is_coin() {
                let special_string = if param.is_balance() {
                    "balance"
                } else {
                    "coin"
                };
                debug!(
                    "Handling {} parameter: {:?} for function: {}::{}::{}",
                    special_string, param, addr, mname, fname
                );
                let ty_args_map = movecall
                    .type_arguments
                    .iter()
                    .enumerate()
                    .map(|(j, ty_arg)| (j as u16, ty_arg.clone()))
                    .collect::<BTreeMap<_, _>>();
                let MoveTypeTag::Struct(s) = param.subst(&ty_args_map).unwrap() else {
                    panic!("Expected {special_string} parameter to be a struct");
                };
                let old_arg =
                    std::mem::replace(arg, SequenceArgument::Result(ptb.commands.len() as u16));
                let input = MoveAbiSignatureToken::U64.gen_input_arg().unwrap();
                inputs.push(input);
                let split_cmd = MoveSequenceCall::Call(MoveCall {
                    module_id: MoveAddress::two(),
                    module_name: special_string.to_string(),
                    function: "split".to_string(),
                    type_arguments: s.tys,
                    arguments: vec![
                        old_arg,
                        SequenceArgument::Input((ptb.inputs.len() + inputs.len()) as u16 - 1),
                    ],
                });
                ptb.commands.push(split_cmd);
            }
        }
    }

    assert_eq!(
        movecall.type_arguments.len(),
        function.type_parameters.len(),
        "Type arguments length mismatch for function: {}::{}::{}",
        addr,
        mname,
        fname
    );

    debug!(
        "Appended function: {:?}, args: {:?}, ty_args: {:?}",
        function_ident, movecall.arguments, movecall.type_arguments
    );

    let arguments = movecall.arguments.clone();
    let returns = if function.return_paramters.len() == 1 {
        vec![SequenceArgument::Result(ptb.commands.len() as u16)]
    } else {
        function
            .return_paramters
            .iter()
            .enumerate()
            .map(|(j, _)| SequenceArgument::NestedResult(ptb.commands.len() as u16, j as u16))
            .collect::<Vec<_>>()
    };
    ptb.commands.push(cmd);
    ptb.inputs.extend(inputs);
    Some((arguments, returns))
}
