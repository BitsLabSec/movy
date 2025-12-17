use std::hash::{Hash, Hasher};

use libafl::{mutators::MutationResult, state::HasRand};
use libafl_bolts::rands::Rand;
use log::{debug, warn};
use movy_types::{
    abi::MoveFunctionAbi,
    input::{FunctionIdent, MoveCall, MoveSequence, MoveSequenceCall, SequenceArgument},
};

use crate::{
    executor::ExecutionExtraOutcome,
    flash::FlashProvider,
    input::MoveInput,
    meta::{HasFuzzMetadata, MutatorKind},
    mutators::mutation_utils::MutableValue,
    state::HasFuzzEnv,
};

pub trait HasFlash {
    fn flash(&self) -> &Option<FlashProvider>;
}

const MAX_STAGE_REPLAY_ATTEMPTS: u64 = 30;

pub enum StageReplayAction {
    Fresh,
    Replay {
        stage_idx: usize,
        snapshot: MoveSequence,
    },
}

pub struct StageReplay {
    kind: MutatorKind,
    cached: Option<MoveSequence>,
    cached_fingerprint: Option<u64>,
    stage_idx: Option<usize>,
    // When attempts exceed MAX_STAGE_REPLAY_ATTEMPTS, restart from stage 0 instead of Fresh
    attempts: u64,
}

impl StageReplay {
    pub fn new(kind: MutatorKind) -> Self {
        Self {
            kind,
            cached: None,
            cached_fingerprint: None,
            stage_idx: None,
            attempts: 0,
        }
    }

    pub fn decide(
        &mut self,
        current_mutator: Option<MutatorKind>,
        outcome: Option<&ExecutionExtraOutcome>,
        current_fingerprint: u64,
    ) -> StageReplayAction {
        if self
            .cached_fingerprint
            .is_some_and(|fp| fp != current_fingerprint)
        {
            self.invalidate_cache();
            self.reset_progress();
            self.cached_fingerprint = Some(current_fingerprint);
            return StageReplayAction::Fresh;
        }
        if self.cached_fingerprint.is_none() {
            self.cached_fingerprint = Some(current_fingerprint);
        }

        if current_mutator != Some(self.kind) {
            self.reset_progress();
            return StageReplayAction::Fresh;
        }

        let Some(outcome) = outcome else {
            self.reset_progress();
            return StageReplayAction::Fresh;
        };
        let Some(stage_idx) = outcome.stage_idx else {
            self.reset_progress();
            return StageReplayAction::Fresh;
        };

        let prev_stage = self.stage_idx;
        self.stage_idx = match self.stage_idx {
            Some(current) if stage_idx > current => Some(current + 1),
            _ => Some(stage_idx),
        };
        if self.stage_idx != prev_stage {
            self.attempts = 0;
        }
        self.attempts += 1;

        if self.attempts >= MAX_STAGE_REPLAY_ATTEMPTS {
            self.restart_progress();
        }

        if let Some(snapshot) = &self.cached {
            StageReplayAction::Replay {
                stage_idx: self.stage_idx.unwrap(),
                snapshot: snapshot.clone(),
            }
        } else {
            StageReplayAction::Fresh
        }
    }

    pub fn record_success(&mut self, ptb: &MoveSequence) {
        self.cached = Some(ptb.clone());
    }

    fn invalidate_cache(&mut self) {
        self.cached = None;
        self.cached_fingerprint = None;
    }

    fn reset_progress(&mut self) {
        self.stage_idx = None;
        self.attempts = 0;
    }

    fn restart_progress(&mut self) {
        self.stage_idx = Some(0);
        self.attempts = 0;
    }
}

pub fn ptb_fingerprint(ptb: &MoveSequence) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    ptb.hash(&mut hasher);
    hasher.finish()
}

pub fn flash_command_limits(mutator: &impl HasFlash) -> (usize, usize) {
    if let Some(provider) = mutator.flash() {
        match provider {
            FlashProvider::Cetus { .. } => (1, 8),
            FlashProvider::Nemo { .. } => (2, 4),
        }
    } else {
        (0, 0)
    }
}

fn command_has_mutable_user_arg(
    function: &MoveFunctionAbi,
    movecall: &MoveCall,
    input_limit: usize,
) -> bool {
    function
        .parameters
        .iter()
        .zip(movecall.arguments.iter())
        .any(|(param, arg)| {
            if param.is_mutable() {
                if let SequenceArgument::Input(input_idx) = arg
                    && (*input_idx as usize) < input_limit
                {
                    return false;
                }
                return true;
            }
            false
        })
}

fn stage_candidate_indices<S: HasFuzzMetadata + HasFuzzEnv>(
    state: &S,
    ptb: &MoveSequence,
    stage_idx: usize,
    input_limit: usize,
) -> Vec<usize> {
    if stage_idx >= ptb.commands.len() {
        warn!(
            "Stage idx {} out of bounds for commands: {:?}",
            stage_idx, ptb.commands
        );
        return vec![];
    }

    let mut indices = Vec::new();
    if let MoveSequenceCall::Call(movecall) = &ptb.commands[stage_idx] {
        let function = state
            .fuzz_state()
            .get_function(
                &movecall.module_id,
                &movecall.module_name,
                &movecall.function,
            )
            .unwrap();
        if command_has_mutable_user_arg(function, movecall, input_limit) {
            indices.push(stage_idx);
        }
        for idx in (0..stage_idx).rev() {
            match &ptb.commands[idx] {
                MoveSequenceCall::Call(prev) if prev.is_split() => indices.push(idx),
                _ => break,
            }
        }
    }
    indices
}

fn general_candidate_indices<S: HasFuzzMetadata + HasFuzzEnv>(
    state: &S,
    ptb: &MoveSequence,
    start_idx: usize,
    input_limit: usize,
) -> Vec<usize> {
    ptb.commands
        .iter()
        .enumerate()
        .skip(start_idx)
        .filter_map(|(idx, cmd)| {
            if let MoveSequenceCall::Call(movecall) = cmd {
                let function = state
                    .fuzz_state()
                    .get_function(
                        &movecall.module_id,
                        &movecall.module_name,
                        &movecall.function,
                    )
                    .unwrap();
                if command_has_mutable_user_arg(function, movecall, input_limit) {
                    Some(idx)
                } else {
                    None
                }
            } else {
                None
            }
        })
        .collect()
}

pub fn candidate_move_call_indices<S: HasFuzzMetadata + HasFuzzEnv>(
    mutator: &impl HasFlash,
    state: &S,
    ptb: &MoveSequence,
    stage_idx: &Option<usize>,
) -> Vec<usize> {
    let (start_idx, input_limit) = flash_command_limits(mutator);
    if let Some(stage_idx) = stage_idx {
        stage_candidate_indices(state, ptb, *stage_idx, input_limit)
    } else {
        general_candidate_indices(state, ptb, start_idx, input_limit)
    }
}

pub fn mutate_ty_arg<S>(
    mutator: &impl HasFlash,
    state: &mut S,
    ptb: &mut MoveSequence,
    stage_idx: &Option<usize>,
) -> MutationResult
where
    S: HasRand + HasFuzzMetadata + HasFuzzEnv,
{
    if ptb.commands.is_empty() {
        return MutationResult::Skipped;
    }
    let (start_idx, _) = flash_command_limits(mutator);
    let mut ty_args = if let Some(stage_idx) = stage_idx {
        let Some(cmd) = ptb.commands.get_mut(*stage_idx) else {
            warn!(
                "Stage idx {} out of bounds for commands: {:?}",
                stage_idx, ptb.commands
            );
            return MutationResult::Skipped;
        };
        let MoveSequenceCall::Call(movecall) = cmd else {
            return MutationResult::Skipped;
        };

        let function = state
            .fuzz_state()
            .get_function(
                &movecall.module_id,
                &movecall.module_name,
                &movecall.function,
            )
            .unwrap()
            .clone();

        let mut results = Vec::new();

        for (i, ty_arg) in movecall.type_arguments.iter_mut().enumerate() {
            if !function
                .parameters
                .iter()
                .any(|param| param.contains_type_param(i as u16))
                && !function
                    .return_paramters
                    .iter()
                    .any(|ret| ret.contains_type_param(i as u16))
            {
                let type_param = *function.type_parameters.get(i).unwrap();
                results.push((type_param, ty_arg));
            }
        }
        results
    } else {
        ptb.commands
            .iter_mut()
            .skip(start_idx)
            .filter_map(|cmd| {
                if let MoveSequenceCall::Call(movecall) = cmd {
                    let function = state
                        .fuzz_state()
                        .get_function(
                            &movecall.module_id,
                            &movecall.module_name,
                            &movecall.function,
                        )
                        .unwrap()
                        .clone();
                    let mut results = Vec::new();

                    for (i, ty_arg) in movecall.type_arguments.iter_mut().enumerate() {
                        if !function
                            .parameters
                            .iter()
                            .chain(function.return_paramters.iter())
                            .any(|param| param.contains_type_param(i as u16))
                            && !function
                                .return_paramters
                                .iter()
                                .any(|ret| ret.contains_type_param(i as u16))
                        {
                            let type_param = *function.type_parameters.get(i).unwrap();
                            results.push((type_param, ty_arg));
                        }
                    }
                    Some(results)
                } else {
                    None
                }
            })
            .flatten()
            .collect::<Vec<_>>()
    };
    if ty_args.is_empty() {
        return MutationResult::Skipped;
    }
    let idx = state.rand_mut().below_or_zero(ty_args.len());
    let (abilities, ty_arg) = ty_args.remove(idx);
    let ability_to_type_tag = &state.fuzz_state().ability_to_type_tag.clone();
    let new_ty_arg = state.rand_mut().choose(
        ability_to_type_tag
            .get(&abilities)
            .cloned()
            .unwrap_or_default(),
    );
    debug!("ty_args mutation candidates: {:?}", ty_args);
    debug!("change ty_arg from {:?} to {:?}", ty_arg, new_ty_arg);
    if new_ty_arg.is_none() {
        return MutationResult::Skipped;
    }
    let new_ty_arg = new_ty_arg.unwrap().clone();
    *ty_arg = new_ty_arg;
    MutationResult::Mutated
}

pub fn mutate_arg<I, S>(
    mutator: &impl HasFlash,
    state: &mut S,
    input: &mut I,
    magic: bool,
    stage_idx: &Option<usize>,
) -> MutationResult
where
    I: MoveInput,
    S: HasRand + HasFuzzMetadata + HasFuzzEnv,
{
    let magic_number_pool = input.magic_number_pool(state.fuzz_state()).clone();
    let ptb = input.sequence_mut();
    if ptb.commands.is_empty() {
        return MutationResult::Skipped;
    }
    let commands = ptb.commands.clone();
    let (_, input_limit) = flash_command_limits(mutator);

    if state.rand_mut().below_or_zero(10) == 0 {
        // 10% chance to mutate type argument
        if mutate_ty_arg(mutator, state, ptb, stage_idx) == MutationResult::Mutated {
            return MutationResult::Mutated;
        }
    }

    let mut result = MutationResult::Skipped;
    let cmd_candidates = candidate_move_call_indices(mutator, state, ptb, stage_idx);
    debug!("cmd_candidates: {:?}", cmd_candidates);
    if cmd_candidates.is_empty() {
        return MutationResult::Skipped;
    }
    let idx = state.rand_mut().below_or_zero(cmd_candidates.len());
    let cmd_idx = *cmd_candidates.get(idx).unwrap();
    let MoveSequenceCall::Call(movecall) = ptb.commands.get_mut(cmd_idx).unwrap() else {
        return MutationResult::Skipped;
    };

    let function = state
        .fuzz_state()
        .get_function(
            &movecall.module_id,
            &movecall.module_name,
            &movecall.function,
        )
        .unwrap();
    let split = movecall.is_split();
    let magic_function_ident = if split {
        let mut function_ident = FunctionIdent::new(
            &movecall.module_id,
            &movecall.module_name,
            &movecall.function,
        );
        for post_cmd in commands.iter().skip(cmd_idx) {
            if let MoveSequenceCall::Call(post_movecall) = post_cmd
                && !post_movecall.is_split()
            {
                function_ident = FunctionIdent::new(
                    &post_movecall.module_id,
                    &post_movecall.module_name,
                    &post_movecall.function,
                );
                break;
            }
        }
        function_ident
    } else {
        FunctionIdent::new(
            &movecall.module_id,
            &movecall.module_name,
            &movecall.function,
        )
    };
    let arg_idx_candidates = function
        .parameters
        .iter()
        .zip(movecall.arguments.iter())
        .enumerate()
        .filter_map(|(i, (param, arg))| {
            if param.is_mutable()
                && matches!(arg, SequenceArgument::Input(input_idx) if *input_idx as usize >= input_limit)
            {
                // Skip struct parameters
                Some(i as u16)
            } else {
                None
            }
        })
        .collect::<Vec<_>>();
    if arg_idx_candidates.is_empty() {
        return MutationResult::Skipped; // No valid arguments to mutate
    }
    let idx = state.rand_mut().choose(&arg_idx_candidates).unwrap();
    let arg = &movecall.arguments.get(*idx as usize).unwrap_or_else(|| {
        panic!(
            "Argument index {} out of bounds for MoveCall command, {:?}",
            idx, movecall
        )
    });
    // Only mutate pure values
    if let SequenceArgument::Input(input_idx) = arg {
        let call_input = &mut ptb.inputs[*input_idx as usize];
        // Mutate the pure value
        let function = state
            .fuzz_state()
            .get_function(
                &movecall.module_id,
                &movecall.module_name,
                &movecall.function,
            )
            .unwrap();
        let param = &function.parameters[*idx as usize];
        if !param.is_mutable() {
            return MutationResult::Skipped;
        }
        let mut new_value = MutableValue::new(call_input.clone());
        if magic {
            new_value.sample_magic_number(
                state,
                &magic_number_pool
                    .get(&magic_function_ident)
                    .cloned()
                    .unwrap_or_default(),
            );
        } else {
            new_value.mutate(
                state,
                &magic_number_pool
                    .get(&magic_function_ident)
                    .cloned()
                    .unwrap_or_default(),
                split,
            );
        }
        *call_input = new_value.value;
        result = MutationResult::Mutated;
    }
    result
}
