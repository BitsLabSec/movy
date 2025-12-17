use std::{collections::BTreeMap, marker::PhantomData};

use libafl::{
    HasMetadata,
    mutators::{MutationResult, Mutator},
    state::HasRand,
};
use libafl_bolts::{Named, rands::Rand};
use log::{debug, trace};
use movy_replay::tracer::{concolic::ConcolicState, op::Log};
use movy_types::input::{FunctionIdent, MoveSequenceCall, SequenceArgument};

use crate::{
    flash::FlashProvider,
    input::MoveInput,
    meta::{HasFuzzMetadata, MutatorKind},
    mutators::utils::{
        HasFlash, StageReplay, StageReplayAction, candidate_move_call_indices,
        flash_command_limits, mutate_arg, ptb_fingerprint,
    },
    solver::solve,
    state::{ExtraNonSerdeFuzzState, HasExtraState, HasFuzzEnv},
};

pub struct ArgMutator<I, S> {
    pub ph: PhantomData<(I, S)>,
    pub flash: Option<FlashProvider>,
    stage: StageReplay,
}

impl<I, S> Default for ArgMutator<I, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I, S> ArgMutator<I, S> {
    pub fn new() -> Self {
        Self {
            ph: PhantomData,
            flash: None,
            stage: StageReplay::new(MutatorKind::Magic),
        }
    }
}

impl<I, S> Named for ArgMutator<I, S> {
    fn name(&self) -> &std::borrow::Cow<'static, str> {
        &std::borrow::Cow::Borrowed("magic_number_mutator")
    }
}

impl<I, S> HasFlash for ArgMutator<I, S> {
    fn flash(&self) -> &Option<FlashProvider> {
        &self.flash
    }
}

pub fn solve_arg<I, S>(
    mutator: &impl HasFlash,
    state: &mut S,
    input: &mut I,
    cmps: &BTreeMap<FunctionIdent, Vec<Log>>,
    solver: &ConcolicState,
    stage_idx: &Option<usize>,
) -> MutationResult
where
    I: MoveInput,
    S: HasRand + HasFuzzMetadata + HasFuzzEnv,
{
    let ptb = input.sequence_mut();
    if ptb.commands.is_empty() {
        return MutationResult::Skipped;
    }
    let (_, input_limit) = flash_command_limits(mutator);
    let mut result = MutationResult::Skipped;
    let cmd_candidates = candidate_move_call_indices(mutator, state, ptb, stage_idx);
    if cmd_candidates.is_empty() {
        return MutationResult::Skipped;
    }
    let idx = state.rand_mut().below_or_zero(cmd_candidates.len());
    let cmd_idx = *cmd_candidates.get(idx).unwrap();
    let MoveSequenceCall::Call(movecall) = ptb.commands.get_mut(cmd_idx).unwrap() else {
        return MutationResult::Skipped;
    };

    let function = {
        let meta = state.fuzz_state();
        meta.get_function(
            &movecall.module_id,
            &movecall.module_name,
            &movecall.function,
        )
        .unwrap()
        .clone()
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
    let Some(solving_arg) = solver.args.get(cmd_idx) else {
        return MutationResult::Skipped;
    };

    let target_function = FunctionIdent::new(
        &movecall.module_id,
        &movecall.module_name,
        &movecall.function,
    );
    let origin_module_id = {
        let meta = state.fuzz_state();
        meta.get_package_metadata(&target_function.0.module_address)
            .unwrap()
            .package_id
    };
    let Some(target_function_logs) = cmps.get(&target_function) else {
        trace!("No logs found for target function: {:?}", target_function);
        return MutationResult::Skipped;
    };
    let mut cmp_constraints = target_function_logs
        .iter()
        .filter_map(|log| match log {
            Log::CmpLog(cmp) => cmp.constraint.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    if !cmp_constraints.is_empty() && state.rand_mut().below_or_zero(2) == 0 {
        // select one constraint to flip
        state
            .rand_mut()
            .choose(&mut cmp_constraints)
            .map(|c| *c = c.not());
    }
    let constraints = cmp_constraints
        .into_iter()
        .chain(target_function_logs.iter().filter_map(|log| match log {
            Log::CastLog(c) => c.constraint.clone(),
            Log::ShlLog(s) => s.constraint.clone(),
            _ => None,
        }))
        .collect::<Vec<_>>();

    let solution = solve(function.clone(), solving_arg, &constraints);
    if let Some(solution) = solution
        && let Some(new_value) = solution.get(&(*idx as usize))
        && let SequenceArgument::Input(input_idx) = arg
    {
        let call_input = &mut ptb.inputs[*input_idx as usize];
        *call_input = new_value.clone();
        result = MutationResult::Mutated;
    }
    result
}

impl<I, S, E> Mutator<I, S> for ArgMutator<I, S>
where
    I: MoveInput,
    S: HasFuzzMetadata
        + HasRand
        + HasMetadata
        + HasFuzzEnv<Env = E>
        + HasExtraState<ExtraState = ExtraNonSerdeFuzzState<E>>,
{
    fn mutate(&mut self, state: &mut S, input: &mut I) -> Result<MutationResult, libafl::Error> {
        // self.flash = input.flash().as_ref().map(|f| f.provider.clone());
        let outcome = state.extra_state().global_outcome.as_ref();
        let extra = outcome.map(|o| &o.extra);
        if let Some(ex) = extra {
            input.update_magic_number(ex);
            debug!(
                "magic mutator current stage: {:?}, last kind: {:?}",
                ex.stage_idx,
                state.fuzz_state().current_mutator
            );
            debug!(
                "magic number pool: {:?}",
                input.magic_number_pool(state.fuzz_state())
            );
        }

        let stage_idx = match self.stage.decide(
            state.fuzz_state().current_mutator,
            extra,
            ptb_fingerprint(input.sequence()),
        ) {
            StageReplayAction::Replay {
                stage_idx,
                snapshot,
            } => {
                *input.sequence_mut() = snapshot;
                Some(stage_idx)
            }
            StageReplayAction::Fresh => None,
        };

        let res = if let Some(ex) = extra.cloned() {
            if state.rand_mut().below_or_zero(2) == 0 {
                solve_arg(self, state, input, &ex.logs, &ex.solver, &stage_idx)
            } else {
                mutate_arg(self, state, input, false, &stage_idx)
            }
        } else {
            mutate_arg(self, state, input, false, &None)
        };

        if res == MutationResult::Mutated {
            self.stage.record_success(input.sequence());
            state.fuzz_state_mut().current_mutator = Some(MutatorKind::Magic);
        }
        *input.outcome_mut() = None;
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
