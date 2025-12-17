use std::{collections::BTreeSet, marker::PhantomData};

use libafl::{
    mutators::{MutationResult, Mutator},
    state::HasRand,
};
use libafl_bolts::{Named, rands::Rand};
use log::debug;
use movy_replay::db::ObjectStoreInfo;
use movy_types::input::{
    InputArgument, MoveCall, MoveSequenceCall, SequenceArgument, SuiObjectInputArgument,
};
use movy_types::object::MoveOwner as Owner;
use sui_types::digests::TransactionDigest;

use crate::{
    input::MoveInput,
    meta::{HasCaller, HasFuzzMetadata},
    mutators::{mutation_utils::MutableValue, object_data::try_construct_args_from_db},
    state::HasFuzzEnv,
};

pub struct HavocMuator<I, S> {
    pub ph: PhantomData<(I, S)>,
}

impl<I, S> Default for HavocMuator<I, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I, S> HavocMuator<I, S> {
    pub fn new() -> Self {
        Self { ph: PhantomData }
    }
}

impl<I, S> Named for HavocMuator<I, S> {
    fn name(&self) -> &std::borrow::Cow<'static, str> {
        &std::borrow::Cow::Borrowed("move_fuzz_mutator")
    }
}

impl<I, S> HavocMuator<I, S> {
    fn mutate_arg(&mut self, state: &mut S, input: &mut I) -> MutationResult
    where
        S: HasRand + HasCaller + HasFuzzMetadata + HasFuzzEnv,
        I: MoveInput,
    {
        let ptb = input.sequence_mut();
        if ptb.commands.is_empty() {
            return MutationResult::Skipped;
        }
        for cmd in ptb.commands.iter_mut() {
            let MoveSequenceCall::Call(movecall) = cmd else {
                continue;
            };
            if movecall.arguments.is_empty() {
                continue;
            }
            let function = state
                .fuzz_state()
                .get_function(
                    &movecall.module_id,
                    &movecall.module_name,
                    &movecall.function,
                )
                .unwrap();
            let arg_idx_candidates = function
                .parameters
                .iter()
                .enumerate()
                .filter_map(|(i, param)| {
                    if param.is_mutable() {
                        // Skip struct parameters
                        Some(i as u16)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();
            if arg_idx_candidates.is_empty() {
                continue; // No valid arguments to mutate
            }
            // Randomly select an argument index to mutate
            let idx = state.rand_mut().below_or_zero(arg_idx_candidates.len());
            let idx = arg_idx_candidates[idx];
            let arg = &mut movecall.arguments[idx as usize];
            // Only mutate pure values
            if let SequenceArgument::Input(input_idx) = arg {
                let input = &mut ptb.inputs[*input_idx as usize];
                // Mutate the pure value
                let function = state
                    .fuzz_state()
                    .get_function(
                        &movecall.module_id,
                        &movecall.module_name,
                        &movecall.function,
                    )
                    .unwrap();
                let param = &function.parameters[idx as usize];
                if !param.is_mutable() {
                    continue;
                }
                let mut new_value = MutableValue::new(input.clone());
                new_value.mutate(state, &BTreeSet::new(), false);
                *input = new_value.value;
            }
        }
        MutationResult::Mutated
    }

    fn mutate_sequence(&mut self, state: &mut S, input: &mut I) -> MutationResult
    where
        I: MoveInput,
        S: HasRand + HasFuzzMetadata + HasFuzzEnv,
    {
        let ptb = input.sequence_mut();
        let inc = if ptb.commands.len() <= 1 {
            true
        } else {
            state.rand_mut().below_or_zero(2) == 0
        };
        if !inc {
            let idx = state.rand_mut().below_or_zero(ptb.commands.len());
            ptb.commands.remove(idx);
            return MutationResult::Mutated;
        }
        let mut selected_times = 0;
        let functions = state
            .fuzz_state()
            .iter_target_functions()
            .map(|(addr, mname, module, fname, function)| {
                (
                    *addr,
                    mname.clone(),
                    module.clone(),
                    fname.clone(),
                    function.clone(),
                )
            })
            .collect::<Vec<_>>();
        loop {
            if selected_times >= 10000 {
                panic!("No suitable function found after 10000 attempts");
            }
            selected_times += 1;
            let (addr, mname, _, fname, function) = functions
                .get(state.rand_mut().below_or_zero(functions.len()))
                .expect("No functions available");
            let mut cmd = MoveSequenceCall::Call(MoveCall {
                module_id: *addr,
                module_name: mname.clone(),
                function: fname.clone(),
                type_arguments: vec![],
                arguments: Vec::from_iter(
                    (ptb.inputs.len()..ptb.inputs.len() + function.parameters.len())
                        .map(|i| SequenceArgument::Input(i as u16)),
                ),
            });
            let MoveSequenceCall::Call(movecall) = &mut cmd else {
                panic!("Expected MoveCall command");
            };
            debug!("Mutating function: {}::{}::{}", addr, mname, fname);
            if let Some((object_ids, ty_args)) = try_construct_args_from_db(movecall, state) {
                movecall.type_arguments = ty_args;
                for (i, param) in function.parameters.iter().enumerate() {
                    if let Some(object_id) = object_ids.get(&(i as u16)) {
                        let SequenceArgument::Input(idx) = movecall.arguments[i] else {
                            // panic!("Expected input argument for object ID");
                            continue;
                        };
                        let Ok(object_info) =
                            state.fuzz_env().inner().get_move_object_info(*object_id)
                        else {
                            debug!("Object ID {:?} not found in DB", object_id);
                            return MutationResult::Skipped;
                        };
                        match object_info.owner {
                            Owner::AddressOwner(_) | Owner::Immutable => {
                                if idx >= ptb.inputs.len() as u16 {
                                    return MutationResult::Skipped;
                                }
                                ptb.inputs[idx as usize] = InputArgument::Object(object_info.ty, {
                                    let digest: TransactionDigest = object_info.digest.into();
                                    SuiObjectInputArgument::imm_or_owned_object(
                                        *object_id,
                                        object_info.version,
                                        digest.into_inner(),
                                    )
                                });
                            }
                            Owner::Shared {
                                initial_shared_version,
                            } => {
                                if idx >= ptb.inputs.len() as u16 {
                                    return MutationResult::Skipped;
                                }
                                ptb.inputs[idx as usize] = InputArgument::Object(
                                    object_info.ty,
                                    SuiObjectInputArgument::shared_object(
                                        *object_id,
                                        initial_shared_version,
                                        true,
                                    ),
                                );
                            }
                            _ => {
                                debug!("Unsupported object owner type: {:?}", object_info.owner);
                                return MutationResult::Skipped;
                            }
                        }
                    } else {
                        if param.is_tx_context() || param.needs_sample() {
                            // Skip tx context parameters
                            ptb.inputs.push(InputArgument::Bool(false));
                            continue;
                        }
                        if let Some(init_value) = param.gen_input_arg() {
                            ptb.inputs.push(init_value);
                        } else {
                            ptb.inputs.push(InputArgument::Bool(false));
                        }
                    }
                }
                ptb.commands.push(cmd);
                break;
            }
        }
        MutationResult::Mutated
    }
}

impl<I, S> Mutator<I, S> for HavocMuator<I, S>
where
    I: MoveInput,
    S: HasFuzzMetadata + HasRand + HasCaller + HasFuzzEnv,
{
    fn mutate(&mut self, state: &mut S, input: &mut I) -> Result<MutationResult, libafl::Error> {
        let ptb = input.sequence();
        if ptb.commands.is_empty() {
            self.mutate_sequence(state, input);
        } else if state.rand_mut().below_or_zero(100) < 20 {
            self.mutate_sequence(state, input);
        }
        let should_havoc = state.rand_mut().below_or_zero(100) < 60;
        let havoc_times = if should_havoc {
            state.rand_mut().below_or_zero(10) + 1
        } else {
            1
        };
        let mut res = MutationResult::Skipped;
        for _ in 0..havoc_times {
            let result = self.mutate_arg(state, input);
            if result == MutationResult::Mutated {
                // *input.outcome_mut() = None;
                res = MutationResult::Mutated;
            }
        }
        Ok(res)
    }

    fn post_exec(
        &mut self,
        _state: &mut S,
        _new_corpus_id: Option<libafl::corpus::CorpusId>,
    ) -> Result<(), libafl::Error> {
        Ok(())
    }
}
