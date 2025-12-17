use itertools::Itertools;
use libafl::state::HasRand;
use libafl_bolts::rands::Rand;
use log::{debug, trace, warn};
use movy_replay::db::ObjectStoreInfo;
use movy_types::object::MoveOwner as Owner;
use movy_types::{
    abi::{MoveAbiSignatureToken, MoveAbility},
    input::{
        FunctionIdent, InputArgument, MoveAddress, MoveCall, MoveSequence, MoveSequenceCall,
        MoveTypeTag, SequenceArgument, SuiObjectInputArgument,
    },
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use sui_types::digests::TransactionDigest;

use crate::{
    r#const::{INIT_FUNCTION_SCORE, SCORE_TICK},
    input::MoveFuzzInput,
    meta::HasFuzzMetadata,
    state::HasFuzzEnv,
};

#[derive(Hash, Debug, Clone, Serialize, Deserialize, Default)]
pub struct ObjectData {
    pub existing_objects: BTreeMap<MoveTypeTag, Vec<(SequenceArgument, Gate)>>,
    pub key_store_objects: Vec<SequenceArgument>,
    pub hot_potatoes: Vec<MoveTypeTag>, // number of times we have sampled an object without the Drop ability
    pub used_object_ids: Vec<MoveAddress>, // used to track object IDs that have been sampled
    pub balances: Vec<SequenceArgument>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
pub enum Gate {
    Owned,
    Immutable,
    Shared,
}

fn owner_matches_gate(owner: &Owner, gate: Gate, current_sender: MoveAddress) -> bool {
    match owner {
        Owner::AddressOwner(addr) => *addr == current_sender,
        Owner::Shared { .. } => true,
        Owner::Immutable => gate == Gate::Immutable,
        _ => false,
    }
}

fn available_objects<S>(ty: &MoveTypeTag, gate: Gate, state: &S) -> Vec<MoveAddress>
where
    S: HasFuzzMetadata + HasFuzzEnv,
    <S as HasFuzzEnv>::Env: ObjectStoreInfo,
{
    let env = state.fuzz_env();
    let meta = state.fuzz_state();
    let current_sender = state.fuzz_state().attacker;
    meta.types_pool
        .get(ty)
        .map(|ids| {
            ids.iter()
                .filter_map(|id| {
                    env.inner()
                        .get_move_object_info(*id)
                        .map(|info| (*id, info))
                        .ok()
                })
                .filter_map(|(id, info)| {
                    if owner_matches_gate(&info.owner, gate, current_sender) {
                        Some(id)
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

pub enum ConstructResult {
    Ok(Vec<SequenceArgument>, Vec<MoveTypeTag>, Vec<InputArgument>),
    PartialFound(
        Vec<Option<SequenceArgument>>,
        BTreeMap<u16, MoveTypeTag>,
        Vec<InputArgument>,
    ),
    Unsolvable,
}

impl ObjectData {
    pub fn new() -> Self {
        Self {
            existing_objects: BTreeMap::new(),
            key_store_objects: Vec::new(),
            hot_potatoes: Vec::new(),
            used_object_ids: vec![],
            balances: Vec::new(),
        }
    }

    pub fn from_ptb<S>(ptb: &MoveSequence, state: &S) -> Self
    where
        S: HasFuzzMetadata + HasFuzzEnv,
    {
        let gas_id = state.fuzz_state().gas_id;
        if ptb.commands.is_empty() {
            let object_data = Self::new();
            return Self {
                used_object_ids: vec![gas_id],
                ..object_data
            };
        }
        let db = state.fuzz_env().inner();
        let mut existing_objects: BTreeMap<MoveTypeTag, Vec<(SequenceArgument, Gate)>> =
            BTreeMap::new();
        let mut key_store_objects = Vec::new();
        let mut hot_potatoes = Vec::new();
        let mut used_object_ids = vec![gas_id];
        let mut balances = Vec::new();
        for (i, input) in ptb.inputs.iter().enumerate() {
            if let InputArgument::Object(_ty, obj) = input {
                let (obj_id, _seq) = match obj {
                    SuiObjectInputArgument::ImmOrOwnedObject((id, seq, _)) => (id, seq),
                    SuiObjectInputArgument::SharedObject {
                        id,
                        initial_shared_version,
                        ..
                    } => (id, initial_shared_version),
                    _ => unimplemented!("Unsupported object argument type"),
                };
                let obj_id = (*obj_id).into();
                used_object_ids.push(obj_id);
                let object = db.get_move_object_info(obj_id).unwrap();
                let gate = match object.owner {
                    Owner::AddressOwner(_) => Gate::Owned,
                    Owner::Immutable => Gate::Immutable,
                    Owner::Shared { .. } => Gate::Shared,
                    _ => {
                        panic!(
                            "Unsupported object owner type: {:?}",
                            db.get_move_object_info(obj_id).unwrap().owner
                        );
                    }
                };
                existing_objects
                    .entry(object.ty)
                    .or_default()
                    .push((SequenceArgument::Input(i as u16), gate));
            }
        }
        for (i, cmd) in ptb.commands.iter().enumerate() {
            let MoveSequenceCall::Call(movecall) = cmd else {
                continue; // Only process MoveCall commands
            };
            let function = state
                .fuzz_state()
                .get_function(
                    &movecall.module_id,
                    &movecall.module_name,
                    &movecall.function,
                )
                .unwrap_or_else(|| {
                    panic!(
                        "Function {}::{}::{} not found in module",
                        movecall.module_id, movecall.module_name, movecall.function
                    )
                });
            let ty_args_map = &movecall
                .type_arguments
                .iter()
                .enumerate()
                .map(|(i, ty)| (i as u16, ty.clone()))
                .collect::<BTreeMap<_, _>>();
            for (arg, param) in movecall.arguments.iter().zip(function.parameters.iter()) {
                if !param.needs_sample() {
                    continue; // Skip parameters that do not need sampling
                }
                if matches!(param, MoveAbiSignatureToken::Reference(_))
                    || matches!(param, MoveAbiSignatureToken::MutableReference(_))
                {
                    continue; // Skip reference parameters
                }
                // if param.has_copy(state.fuzz_state()) {
                //     continue; // Skip parameters that have the Copy ability
                // }
                let instantiated_param = param.subst(ty_args_map).unwrap();
                existing_objects
                    .get_mut(&instantiated_param)
                    .unwrap_or_else(|| {
                        panic!(
                            "Expected existing objects for command {}, parameter type {}, input {}",
                            i, instantiated_param, ptb,
                        )
                    })
                    .retain(|(a, _)| *a != *arg);
                if existing_objects
                    .get(&instantiated_param)
                    .unwrap()
                    .is_empty()
                {
                    existing_objects.remove(&instantiated_param); // Remove empty entries
                }
                if matches!(arg, SequenceArgument::Input(_)) {
                    continue; // Skip input arguments for key store and hot potatoes
                }
                if param.is_balance() {
                    balances.retain(|a| *a != *arg); // Remove from balances if it is a balance object
                }
                if param.is_key_store() {
                    key_store_objects.retain(|a| *a != *arg); // Remove from key_store_objects if it is a key store object
                }
                if param.is_hot_potato() {
                    hot_potatoes.remove(
                        hot_potatoes
                            .iter()
                            .position(|x| x == &instantiated_param)
                            .unwrap_or_else(|| {
                                panic!(
                                    "Expected hot potato object for type {:?}, input {}",
                                    instantiated_param,
                                    MoveFuzzInput {
                                        sequence: ptb.clone(),
                                        ..Default::default()
                                    }
                                )
                            }),
                    );
                }
            }

            for (j, ret_ty) in function.return_paramters.iter().enumerate() {
                if let MoveAbiSignatureToken::Vector(inner) = ret_ty {
                    let instantiated_ret_ty = ret_ty.subst(ty_args_map).unwrap();
                    match inner.as_ref() {
                        MoveAbiSignatureToken::Struct(inner)
                        | MoveAbiSignatureToken::StructInstantiation(inner, _) => {
                            if !inner.abilities.contains(MoveAbility::DROP) {
                                hot_potatoes.push(instantiated_ret_ty);
                            }
                        }
                        MoveAbiSignatureToken::TypeParameter(idx, _) => {
                            let ty_tag = ty_args_map.get(idx).unwrap();
                            let MoveTypeTag::Struct(tag) = ty_tag else {
                                continue;
                            };
                            let abilities = state
                                .fuzz_state()
                                .get_abilities(
                                    &tag.address,
                                    &tag.module.to_string(),
                                    &tag.name.to_string(),
                                )
                                .unwrap_or_else(|| {
                                    panic!(
                                        "Struct {}::{}::{} not found in module",
                                        tag.address, tag.module, tag.name
                                    )
                                });
                            if !abilities.contains(MoveAbility::DROP) {
                                hot_potatoes.push(instantiated_ret_ty);
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                let instantiated_ret_ty = ret_ty.subst(ty_args_map).unwrap();
                if !matches!(instantiated_ret_ty, MoveTypeTag::Struct(_)) {
                    continue; // Only process struct return types
                }
                let res_arg = if function.return_paramters.len() == 1 {
                    SequenceArgument::Result(i as u16)
                } else {
                    SequenceArgument::NestedResult(i as u16, j as u16)
                };
                existing_objects
                    .entry(instantiated_ret_ty.clone())
                    .or_default()
                    .push((res_arg, Gate::Owned));
                if ret_ty.is_balance() {
                    balances.push(res_arg);
                }
                if ret_ty.is_key_store() {
                    key_store_objects.push(res_arg);
                }
                if ret_ty.is_hot_potato() {
                    hot_potatoes.push(instantiated_ret_ty);
                }
            }
        }
        Self {
            existing_objects,
            hot_potatoes,
            key_store_objects,
            used_object_ids,
            balances,
        }
    }

    pub fn from_ptb_and_remove_used(
        ptb: &MoveSequence,
        state: &(impl HasFuzzMetadata + HasFuzzEnv),
        used_arguments: &Vec<SequenceArgument>,
    ) -> Self {
        let mut data = Self::from_ptb(ptb, state);
        for arg in used_arguments.iter() {
            for (ty_tag, candidates) in data.existing_objects.iter_mut() {
                if candidates.iter().all(|(a, _)| a != arg) {
                    continue;
                }
                candidates.retain(|(a, _)| a != arg);
                if let Some(idx) = data.hot_potatoes.iter().position(|x| x == ty_tag) {
                    data.hot_potatoes.remove(idx);
                }
            }
            data.existing_objects
                .retain(|_, candidates| !candidates.is_empty());
            data.balances.retain(|a| a != arg);
            data.key_store_objects.retain(|a| a != arg);
        }
        data
    }
}

fn intersect_generics(
    type_args: &[&BTreeMap<u16, MoveTypeTag>],
) -> Option<BTreeMap<u16, MoveTypeTag>> {
    let mut result = BTreeMap::new();
    for type_arg in type_args.iter() {
        for (index, ty_tag) in type_arg.iter() {
            if let Some(existing_ty_tag) = result.get(index) {
                if existing_ty_tag != ty_tag {
                    return None; // Found a conflict in type arguments
                }
            } else {
                result.insert(*index, ty_tag.clone());
            }
        }
    }
    Some(result)
}

fn get_ty_args_candidates(
    rows: &[Vec<BTreeMap<u16, MoveTypeTag>>],
) -> Vec<BTreeMap<u16, MoveTypeTag>> {
    rows.iter()
        .map(|r| r.iter())
        .multi_cartesian_product()
        .filter_map(|combo| intersect_generics(&combo))
        .collect()
}

pub fn gen_type_tag_by_abilities<S>(abilities: &MoveAbility, state: &mut S) -> MoveTypeTag
where
    S: HasFuzzMetadata + HasRand + HasFuzzEnv,
{
    let ability_to_type_tag = state.fuzz_state().ability_to_type_tag.clone();
    ability_to_type_tag
        .get(abilities)
        .and_then(|type_tags| {
            if type_tags.is_empty() {
                None
            } else {
                Some(type_tags[state.rand_mut().below_or_zero(type_tags.len())].clone())
            }
        })
        .unwrap_or(MoveTypeTag::U64) // Default to U64 if no type tag is found
}

fn get_type_tag_ability(
    type_tag: &MoveTypeTag,
    state: &(impl HasFuzzMetadata + HasFuzzEnv),
) -> MoveAbility {
    match type_tag {
        MoveTypeTag::Bool => MoveAbility::PRIMITIVES,
        MoveTypeTag::Address => MoveAbility::PRIMITIVES,
        MoveTypeTag::U8 => MoveAbility::PRIMITIVES,
        MoveTypeTag::U16 => MoveAbility::PRIMITIVES,
        MoveTypeTag::U32 => MoveAbility::PRIMITIVES,
        MoveTypeTag::U64 => MoveAbility::PRIMITIVES,
        MoveTypeTag::U128 => MoveAbility::PRIMITIVES,
        MoveTypeTag::U256 => MoveAbility::PRIMITIVES,
        MoveTypeTag::Signer => MoveAbility::DROP,
        MoveTypeTag::Vector(_) => MoveAbility::PRIMITIVES,
        MoveTypeTag::Struct(tag) => state
            .fuzz_state()
            .get_abilities(&tag.address, &tag.module.to_string(), &tag.name.to_string())
            .unwrap_or_else(|| {
                panic!(
                    "Struct {}::{}::{} not found in module, meta: {:?}",
                    tag.address,
                    tag.module,
                    tag.name,
                    state.fuzz_state().module_address_to_package
                )
            }),
    }
}

// return a map of ty param index to type arguments
fn try_sample_object_from_db<S>(
    ty: &MoveAbiSignatureToken,
    db_type_tags: &[MoveTypeTag],
    function_abilities: &[MoveAbility],
    gate: Gate,
    state: &mut S,
) -> Vec<BTreeMap<u16, MoveTypeTag>>
where
    S: HasFuzzMetadata + HasRand + HasFuzzEnv,
    <S as HasFuzzEnv>::Env: ObjectStoreInfo,
{
    let mut param_ty_arg_candidates: Vec<BTreeMap<u16, MoveTypeTag>> = Vec::new();
    for db_ty_tag in db_type_tags.iter() {
        if let Some(ty_args) = ty.extract_ty_args(db_ty_tag) {
            if ty_args.iter().any(|(idx, ty_tag)| {
                let ability_set = get_type_tag_ability(ty_tag, state);
                !function_abilities[*idx as usize].is_subset_of(&ability_set)
            }) {
                continue; // Skip if any type argument does not match the function's abilities
            }
            let MoveTypeTag::Struct(_) = db_ty_tag else {
                panic!("Expected a struct type tag");
            };
            let env = state.fuzz_env();
            let meta = state.fuzz_state();
            let Some(addresses) = meta.types_pool.get(db_ty_tag) else {
                continue;
            };
            let current_sender = state.fuzz_state().attacker;
            let has_gate_match = addresses.iter().any(|addr| {
                if let Ok(info) = env.inner().get_move_object_info(*addr) {
                    owner_matches_gate(&info.owner, gate, current_sender)
                } else {
                    false
                }
            });
            if !has_gate_match {
                continue;
            }
            param_ty_arg_candidates.push(ty_args);
        } else {
            trace!(
                "Failed to extract type arguments for type {:?} from db type tag {:?}",
                ty, db_ty_tag
            );
        }
    }
    param_ty_arg_candidates
}

// return a map of ty param index to type arguments
fn try_sample_object_from_cache<S>(
    existing_objects: &BTreeMap<MoveTypeTag, Vec<(SequenceArgument, Gate)>>,
    ty: &MoveAbiSignatureToken,
    function_abilities: &[MoveAbility],
    gate: Gate,
    state: &mut S,
) -> Vec<BTreeMap<u16, MoveTypeTag>>
where
    S: HasFuzzMetadata + HasRand + HasFuzzEnv,
{
    let mut param_ty_arg_candidates: Vec<BTreeMap<u16, MoveTypeTag>> = Vec::new();
    for (db_ty_tag, candidates) in existing_objects.iter() {
        if let Some(ty_args) = ty.extract_ty_args(db_ty_tag) {
            if ty_args.iter().any(|(idx, ty_tag)| {
                let ability_set = get_type_tag_ability(ty_tag, state);
                !function_abilities[*idx as usize].is_subset_of(&ability_set)
            }) {
                continue; // Skip if any type argument does not match the function's abilities
            }
            let MoveTypeTag::Struct(_) = db_ty_tag.clone() else {
                panic!("Expected a struct type tag");
            };
            match gate {
                Gate::Owned => {
                    if candidates.iter().all(|(_, gate)| gate != &Gate::Owned) {
                        continue; // Skip if no owned objects are found
                    }
                }
                Gate::Immutable => {}
                Gate::Shared => {
                    if candidates.iter().all(|(_, gate)| gate == &Gate::Immutable) {
                        continue; // Skip if no shared objects are found
                    }
                }
            }
            param_ty_arg_candidates.push(ty_args)
        }
    }
    param_ty_arg_candidates
}

// return a map of parameter index to object ID and type arguments
pub fn try_construct_args_from_db<S>(
    movecall: &MoveCall,
    state: &mut S,
) -> Option<(BTreeMap<u16, MoveAddress>, Vec<MoveTypeTag>)>
where
    S: HasFuzzMetadata + HasRand + HasFuzzEnv,
{
    let meta = state.fuzz_state();
    let function = meta
        .get_function(
            &movecall.module_id,
            &movecall.module_name,
            &movecall.function,
        )
        .unwrap()
        .clone();
    let db_type_tags = meta
        .types_pool
        .keys()
        .cloned()
        .collect::<Vec<MoveTypeTag>>();
    debug!(
        "Sampling objects for function {}::{}::{}",
        movecall.module_id, movecall.module_name, movecall.function
    );

    let function_abilities = function.type_parameters.to_vec();
    let param_with_gate: Vec<(u16, MoveAbiSignatureToken, Gate)> = function
        .parameters
        .iter()
        .enumerate()
        .filter_map(|(i, param)| {
            if param.is_tx_context() {
                return None; // Skip TxContext parameters
            };
            match param {
                MoveAbiSignatureToken::Struct { .. }
                | MoveAbiSignatureToken::StructInstantiation(_, _) => {
                    Some((i as u16, param.clone(), Gate::Owned))
                }
                MoveAbiSignatureToken::Reference(b) => match b.as_ref() {
                    MoveAbiSignatureToken::Struct { .. }
                    | MoveAbiSignatureToken::StructInstantiation(_, _) => {
                        Some((i as u16, b.as_ref().clone(), Gate::Immutable))
                    }
                    _ => None,
                },
                MoveAbiSignatureToken::MutableReference(b) => match b.as_ref() {
                    MoveAbiSignatureToken::Struct { .. }
                    | MoveAbiSignatureToken::StructInstantiation(_, _) => {
                        Some((i as u16, b.as_ref().clone(), Gate::Shared))
                    }
                    _ => None,
                },
                _ => None,
            }
        })
        .collect();

    // generate type argument candidates for each parameter
    let mut ty_args_candidates: Vec<Vec<BTreeMap<u16, MoveTypeTag>>> = Vec::new();
    for (i, param, gate) in param_with_gate.iter() {
        let param_ty_arg_candidates =
            try_sample_object_from_db(param, &db_type_tags, &function_abilities, *gate, state);
        if param_ty_arg_candidates.is_empty() {
            // If no candidates are found for this parameter, we cannot proceed
            debug!(
                "No candidates found for parameter {} with type {:?} and gate {:?}",
                i, param, gate
            );
            return None;
        };
        ty_args_candidates.push(param_ty_arg_candidates);
    }

    // generate type arguments based on the candidates
    let ty_args_candidates = get_ty_args_candidates(&ty_args_candidates);
    if ty_args_candidates.is_empty() {
        debug!("No valid type argument candidates found for the function");
        return None; // If no valid type argument candidates are found, we cannot proceed
    }
    let ty_args_map = &ty_args_candidates[state.rand_mut().below_or_zero(ty_args_candidates.len())];
    let ty_args = (0..function_abilities.len())
        .map(|i| {
            ty_args_map
                .get(&(i as u16))
                .cloned()
                .unwrap_or_else(|| gen_type_tag_by_abilities(&function_abilities[i], state))
        })
        .collect::<Vec<_>>();

    // Substitute the type arguments into the parameters and sample objects
    let mut object_candidates = BTreeMap::new();
    for (i, param, gate) in param_with_gate.iter() {
        let instantiated_ty = param
            .subst(ty_args_map)
            .expect("Failed to substitute type arguments");
        let MoveTypeTag::Struct(tag) = instantiated_ty.clone() else {
            panic!("Expected a struct type tag for parameter {}", i);
        };
        debug!(
            "Sampling objects for parameter {} with type {:?} and gate {:?}",
            i, tag, gate
        );
        let object_ids = available_objects(&instantiated_ty, *gate, state);
        object_candidates.insert(*i, object_ids);
    }
    if object_candidates.iter().any(|(_, ids)| ids.is_empty()) {
        debug!("No object candidates found for some parameters");
        return None;
    }
    let objects = object_candidates
        .into_iter()
        .map(|(index, ids)| {
            if ids.is_empty() {
                panic!(
                    "No objects found for parameter {} with type {:?} and gate {:?}",
                    index,
                    param_with_gate
                        .iter()
                        .find(|(i, _, _)| *i == index)
                        .map(|(_, ty, _)| ty),
                    param_with_gate
                        .iter()
                        .find(|(i, _, _)| *i == index)
                        .map(|(_, _, gate)| gate)
                );
            }
            let id = ids[state.rand_mut().below_or_zero(ids.len())];
            (index, id)
        })
        .collect::<BTreeMap<u16, MoveAddress>>();

    Some((objects, ty_args.into_iter().collect()))
}

// return a map of parameter index to argument and type arguments
pub fn try_construct_args<S>(
    input_len: usize,
    params: &Vec<MoveAbiSignatureToken>,
    function_abilities: &[MoveAbility],
    fixed_ty_args: &BTreeMap<u16, MoveTypeTag>,
    object_data: &ObjectData,
    state: &mut S,
) -> ConstructResult
where
    S: HasFuzzMetadata + HasRand + HasFuzzEnv,
    <S as HasFuzzEnv>::Env: ObjectStoreInfo,
{
    let mut existing_objects = object_data.existing_objects.clone();
    let mut used_object_ids = object_data.used_object_ids.clone();
    let db_type_tags = {
        let meta = state.fuzz_state();
        meta.types_pool
            .keys()
            .cloned()
            .collect::<Vec<MoveTypeTag>>()
    };

    if params.iter().any(|param| match param {
        MoveAbiSignatureToken::Struct { .. } | MoveAbiSignatureToken::StructInstantiation(_, _) => {
            false
        }
        MoveAbiSignatureToken::Reference(b) => match b.as_ref() {
            MoveAbiSignatureToken::Struct { .. }
            | MoveAbiSignatureToken::StructInstantiation(_, _) => false,
            _ => true,
        },
        MoveAbiSignatureToken::MutableReference(b) => match b.as_ref() {
            MoveAbiSignatureToken::Struct { .. }
            | MoveAbiSignatureToken::StructInstantiation(_, _) => false,
            _ => true,
        },
        _ => true,
    }) {
        debug!("Skipping function with vector of struct parameters or type parameters");
        return ConstructResult::Unsolvable; // Skip functions with reference or mutable reference parameters
    }

    let param_with_gate: Vec<(MoveAbiSignatureToken, Gate)> = params
        .iter()
        .map(|param| {
            if param.is_tx_context() {
                panic!("should not be tx context"); // Skip TxContext parameters
            };
            let param = param.partial_subst(fixed_ty_args);
            match param {
                MoveAbiSignatureToken::Struct { .. }
                | MoveAbiSignatureToken::StructInstantiation(_, _) => (param.clone(), Gate::Owned),
                MoveAbiSignatureToken::Reference(b) => match b.as_ref() {
                    MoveAbiSignatureToken::Struct { .. }
                    | MoveAbiSignatureToken::StructInstantiation(_, _) => {
                        (b.as_ref().clone(), Gate::Immutable)
                    }
                    _ => unreachable!(),
                },
                MoveAbiSignatureToken::MutableReference(b) => match b.as_ref() {
                    MoveAbiSignatureToken::Struct { .. }
                    | MoveAbiSignatureToken::StructInstantiation(_, _) => {
                        (b.as_ref().clone(), Gate::Shared)
                    }
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            }
        })
        .collect();

    // generate type argument candidates for each parameter
    let mut ty_args_candidates: Vec<Vec<BTreeMap<u16, MoveTypeTag>>> = vec![];
    let mut missing_idx = vec![];
    for (i, (param, gate)) in param_with_gate.iter().enumerate() {
        let param_ty_arg_candidates_from_cache = try_sample_object_from_cache(
            &existing_objects,
            param,
            function_abilities,
            *gate,
            state,
        );
        let param_ty_arg_candidates_from_db = if param.ability().unwrap().contains(MoveAbility::KEY)
        {
            try_sample_object_from_db(param, &db_type_tags, function_abilities, *gate, state)
        } else {
            vec![]
        };
        let param_ty_arg_candidates = param_ty_arg_candidates_from_cache
            .into_iter()
            .chain(param_ty_arg_candidates_from_db.into_iter())
            .collect::<Vec<_>>();

        if param_ty_arg_candidates.is_empty() {
            debug!(
                "No candidates found for parameter {:?} and gate {:?}",
                param, gate
            );
            missing_idx.push(i as u16);
            continue;
        }

        ty_args_candidates.push(param_ty_arg_candidates);
    }

    // generate type arguments based on the candidates
    let ty_args_candidates = get_ty_args_candidates(&ty_args_candidates);
    if ty_args_candidates.is_empty() {
        debug!("No valid type argument candidates found for the function");
        return ConstructResult::Unsolvable; // If no valid type argument candidates are found, we cannot proceed
    }
    let ty_args_map = &ty_args_candidates[state.rand_mut().below_or_zero(ty_args_candidates.len())];

    // Substitute the type arguments into the parameters and sample objects
    let mut return_args = Vec::new();
    let mut inputs = Vec::new();
    for (i, (param, gate)) in param_with_gate.iter().enumerate() {
        if missing_idx.contains(&(i as u16)) {
            debug!(
                "Skipping parameter {} with type {:?} and gate {:?} due to missing candidates",
                i, param, gate
            );
            return_args.push(None);
            continue; // Skip parameters that have no candidates
        }
        let instantiated_ty = param
            .subst(ty_args_map)
            .expect("Failed to substitute type arguments");
        let MoveTypeTag::Struct(tag) = instantiated_ty.clone() else {
            panic!("Expected a struct type tag for parameter");
        };
        if !matches!(
            param,
            MoveAbiSignatureToken::Struct { .. } | MoveAbiSignatureToken::StructInstantiation(_, _)
        ) {
            panic!("Expected a struct type for parameter");
        };
        debug!("Sampling objects for parameter {} and gate {:?}", tag, gate);
        if let Some(args) = existing_objects.get_mut(&instantiated_ty) {
            let filtered_idxes: Vec<usize> = args
                .iter()
                .enumerate()
                .filter(|(_, (_, g))| match gate {
                    Gate::Owned => g != &Gate::Immutable,
                    Gate::Immutable => true,
                    Gate::Shared => g != &Gate::Immutable,
                })
                .map(|(i, _)| i)
                .collect();
            if !filtered_idxes.is_empty() {
                let (arg, _) = if
                // !param.ability(state.fuzz_state()).has_copy() ignore Copy ability
                gate == &Gate::Owned {
                    args.remove(
                        filtered_idxes[state.rand_mut().below_or_zero(filtered_idxes.len())],
                    )
                } else {
                    *args
                        .get(filtered_idxes[state.rand_mut().below_or_zero(filtered_idxes.len())])
                        .unwrap()
                };
                return_args.push(Some(arg));
                continue;
            } else {
                debug!(
                    "No existing objects found for parameter {:?} and gate {:?}",
                    param, gate
                );
                return_args.push(None);
                missing_idx.push(i as u16);
                continue; // If no objects are found, skip this parameter
            }
        }

        // Sample an object from the db
        let object_ids = available_objects(&instantiated_ty, *gate, state);
        let object_ids = object_ids
            .into_iter()
            .filter(|id| !used_object_ids.contains(id))
            .collect::<Vec<_>>();
        if object_ids.is_empty() {
            debug!(
                "No objects found in DB for tag {:?} and gate {:?}",
                param, gate
            );
            return_args.push(None);
            missing_idx.push(i as u16);
            continue; // If no objects are found, skip this parameter
        }

        let object_id = object_ids[state.rand_mut().below_or_zero(object_ids.len())];
        used_object_ids.push(object_id);
        let object = {
            let env = state.fuzz_env();
            match env.inner().get_move_object_info(object_id) {
                Ok(obj) => obj,
                Err(e) => {
                    warn!(
                        "Failed to get object info for object ID {:?}: {:?}",
                        object_id, e
                    );
                    return ConstructResult::Unsolvable;
                }
            }
        };
        match object.owner {
            Owner::AddressOwner(_) | Owner::Immutable => {
                let digest: TransactionDigest = object.digest.into();
                inputs.push(InputArgument::Object(
                    object.ty,
                    SuiObjectInputArgument::imm_or_owned_object(
                        object_id,
                        object.version,
                        digest.into_inner(),
                    ),
                ));
            }
            Owner::Shared {
                initial_shared_version,
            } => {
                inputs.push(InputArgument::Object(
                    object.ty,
                    SuiObjectInputArgument::shared_object(object_id, initial_shared_version, true),
                ));
            }
            _ => {
                warn!("Unsupported object owner type: {:?}", object.owner);
                return ConstructResult::Unsolvable;
            }
        }
        let arg = SequenceArgument::Input((input_len + inputs.len() - 1) as u16);
        return_args.push(Some(arg));
    }

    if missing_idx.is_empty() {
        let ty_args = (0..function_abilities.len())
            .map(|i| {
                ty_args_map.get(&(i as u16)).cloned().unwrap_or_else(|| {
                    fixed_ty_args
                        .get(&(i as u16))
                        .unwrap_or(&gen_type_tag_by_abilities(&function_abilities[i], state))
                        .clone()
                })
            })
            .collect::<Vec<_>>();
        ConstructResult::Ok(return_args.into_iter().flatten().collect(), ty_args, inputs)
    } else {
        ConstructResult::PartialFound(
            return_args,
            ty_args_map
                .iter()
                .chain(fixed_ty_args.iter())
                .map(|(k, v)| (*k, v.clone()))
                .collect(),
            inputs,
        )
    }
}

pub fn update_score<S>(hot_potatoes: &Vec<MoveTypeTag>, state: &mut S)
where
    S: HasFuzzMetadata + HasFuzzEnv,
{
    let mut increased_functions = BTreeSet::new();
    let consumers: Vec<FunctionIdent> = {
        let meta_state = state.fuzz_state();
        hot_potatoes
            .iter()
            .flat_map(|type_tag| {
                meta_state
                    .type_graph
                    .find_consumers(&MoveAbiSignatureToken::from_type_tag(type_tag))
                    .iter()
                    .map(|(module_id, consumer_function)| {
                        FunctionIdent::new(
                            &module_id.module_address,
                            &module_id.module_name,
                            &consumer_function.name,
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    {
        let meta = state.fuzz_state_mut();
        for func in consumers {
            meta.function_scores
                .entry(func.clone())
                .and_modify(|score| *score += SCORE_TICK);
            increased_functions.insert(func);
        }
    }
    let specific_function_scores = state.fuzz_state().specific_function_scores.clone();
    let meta = state.fuzz_state_mut();
    for (func, score) in meta.function_scores.iter_mut() {
        if !increased_functions.contains(func) {
            if let Some(s) = specific_function_scores.get(func) {
                *score = *s;
            } else {
                *score = INIT_FUNCTION_SCORE;
            }
        }
    }
}
