use std::{collections::BTreeMap, marker::PhantomData};

use libafl::{
    HasMetadata,
    mutators::{MutationResult, Mutator},
    state::HasRand,
};
use libafl_bolts::{Named, rands::Rand};
use log::debug;
use movy_types::input::{MoveSequence, MoveSequenceCall, SequenceArgument};

use crate::{
    r#const::{ADD_MOVECALL_PROB, INIT_FUNCTION_SCORE},
    flash::FlashProvider,
    input::MoveInput,
    meta::{FuzzMetadata, HasFuzzMetadata, MutatorKind},
    mutators::{
        object_data::ObjectData,
        utils::{HasFlash, StageReplay},
    },
    mutators::{
        object_data::update_score,
        utils::{StageReplayAction, mutate_arg, ptb_fingerprint},
    },
    state::{ExtraNonSerdeFuzzState, HasExtraState, HasFuzzEnv},
};

mod append;
mod hooks;
mod post;
mod remap;

use append::{append_function, weighted_sample};
use hooks::{apply_hooks, strip_generated};

pub struct SequenceMutator<I, S> {
    pub ph: PhantomData<(I, S)>,
    pub flash: Option<FlashProvider>,
    stage: StageReplay,
}

impl<I, S> Default for SequenceMutator<I, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I, S> SequenceMutator<I, S> {
    pub fn new() -> Self {
        Self {
            ph: PhantomData,
            flash: None,
            stage: StageReplay::new(MutatorKind::Sequence),
        }
    }
}

impl<I, S> Named for SequenceMutator<I, S> {
    fn name(&self) -> &std::borrow::Cow<'static, str> {
        &std::borrow::Cow::Borrowed("sequence_mutator")
    }
}

impl<I, S> HasFlash for SequenceMutator<I, S> {
    fn flash(&self) -> &Option<FlashProvider> {
        &self.flash
    }
}

impl<I, S> SequenceMutator<I, S> {
    fn gen_remove_idxes(
        &self,
        meta: &FuzzMetadata,
        ptb: &MoveSequence,
        idx: u16,
        removed_idxes: &mut Vec<u16>,
    ) where
        S: HasFuzzEnv,
    {
        if let Some(provider) = &self.flash {
            match provider {
                FlashProvider::Cetus { .. } => {
                    if idx as usize >= ptb.commands.len() - 3 || idx == 0 {
                        // We cannot remove the first or last commands (flash & repay)
                        return;
                    }
                }
                FlashProvider::Nemo { .. } => {
                    if idx as usize >= ptb.commands.len() - 1 || idx <= 1 {
                        return;
                    }
                }
            }
        } else if idx as usize >= ptb.commands.len() {
            return;
        }

        removed_idxes.push(idx);

        let mut deleting_idxes = vec![];
        for (post_idx, post_cmd) in ptb.commands.iter().skip(idx as usize + 1).enumerate() {
            if let MoveSequenceCall::Call(movecall) = post_cmd {
                let mut need_remove = false;
                for arg in movecall.arguments.iter() {
                    if matches!(arg, SequenceArgument::Result(i) | SequenceArgument::NestedResult(i, _) if *i == idx)
                    {
                        need_remove = true;
                        break; // No need to check further arguments
                    }
                    // substitude not implemented yet
                }
                if need_remove {
                    deleting_idxes.push(idx + post_idx as u16 + 1);
                }
            }
        }
        deleting_idxes.sort_unstable();
        deleting_idxes.dedup();
        for idx in deleting_idxes.iter().rev() {
            if removed_idxes.contains(idx) {
                continue;
            }
            self.gen_remove_idxes(meta, ptb, *idx, removed_idxes);
        }

        let cmd = ptb.commands.get(idx as usize).unwrap();
        let MoveSequenceCall::Call(movecall) = cmd else {
            return;
        };
        for i in (0..idx).rev() {
            if removed_idxes.contains(&i) {
                continue;
            }
            if ptb
                .commands
                .get(i as usize)
                .is_some_and(|c| matches!(c, MoveSequenceCall::Call(mc) if mc.is_split()))
            {
                self.gen_remove_idxes(meta, ptb, i, removed_idxes);
            } else {
                break;
            }
        }
        let mut deleting_prev_idxes = vec![];
        let function = meta
            .get_function(
                &movecall.module_id,
                &movecall.module_name,
                &movecall.function,
            )
            .unwrap();
        for (arg, param) in movecall.arguments.iter().zip(function.parameters.iter()) {
            if matches!(
                param,
                movy_types::abi::MoveAbiSignatureToken::Struct { .. }
                    | movy_types::abi::MoveAbiSignatureToken::StructInstantiation(_, _)
            ) && param.is_hot_potato()
            {
                if let SequenceArgument::Result(i) = arg {
                    deleting_prev_idxes.push(*i);
                } else if let SequenceArgument::NestedResult(i, _j) = arg {
                    deleting_prev_idxes.push(*i);
                }
            }
        }
        deleting_prev_idxes.sort_unstable();
        deleting_prev_idxes.dedup();
        for i in deleting_prev_idxes.iter().rev() {
            if removed_idxes.contains(i) {
                continue;
            }
            self.gen_remove_idxes(meta, ptb, *i, removed_idxes);
        }

        removed_idxes.sort_unstable();
        removed_idxes.dedup();
    }

    fn remove_command(
        &self,
        meta: &FuzzMetadata,
        ptb: &mut MoveSequence,
        idx: u16,
    ) -> MutationResult
    where
        I: MoveInput,
        S: HasRand + HasFuzzMetadata + HasFuzzEnv,
    {
        debug!("Removing command at index {}", idx);
        let mut removed_idxes = vec![];
        self.gen_remove_idxes(meta, ptb, idx, &mut removed_idxes);
        debug!(
            "Also removing dependent commands at indexes {:?}",
            removed_idxes
        );
        if removed_idxes.len() >= ptb.commands.len() || removed_idxes.is_empty() {
            // We cannot remove all commands
            return MutationResult::Skipped;
        }

        for remove_idx in removed_idxes.iter().rev() {
            ptb.commands.remove(*remove_idx as usize);
        }
        for cmd in ptb.commands.iter_mut() {
            if let MoveSequenceCall::Call(movecall) = cmd {
                movecall.arguments.iter_mut().for_each(|arg| match arg {
                    SequenceArgument::Result(i) => {
                        let mut shift = 0;
                        for removed_idx in removed_idxes.iter() {
                            if *removed_idx < *i {
                                shift += 1;
                            }
                        }
                        *i -= shift;
                    }
                    SequenceArgument::NestedResult(i, _j) => {
                        let mut shift = 0;
                        for removed_idx in removed_idxes.iter() {
                            if *removed_idx < *i {
                                shift += 1;
                            }
                        }
                        *i -= shift;
                    }
                    _ => {}
                });
            }
        }

        MutationResult::Mutated
    }

    fn finish(&self, _state: &mut S, _ptb: &mut MoveSequence)
    where
        I: MoveInput,
        S: HasFuzzMetadata + HasRand + HasFuzzEnv,
    {
        if let Some(_provider) = &self.flash {
            // match provider {
            //     FlashProvider::Cetus { coin_a, coin_b, .. } => {
            //         // Ensure repay coins are set empty
            //         *provider.repay_coin0(ptb).unwrap() =
            //             SequenceArgument::Result(ptb.commands.len() as u16 - 3);
            //         *provider.repay_coin1(ptb).unwrap() =
            //             SequenceArgument::Result(ptb.commands.len() as u16 - 2);
            //         // Set split coins to to flash coin first, trying to sample from other functions
            //         *provider.repay_split_coin0(ptb).unwrap() =
            //             SequenceArgument::NestedResult(0, 0);
            //         *provider.repay_split_coin1(ptb).unwrap() =
            //             SequenceArgument::NestedResult(0, 1);
            //         let mut object_data = ObjectData::from_ptb(ptb, state, &self.db);
            //         object_data
            //             .existing_objects
            //             .iter_mut()
            //             .for_each(|(_, objs)| {
            //                 objs.retain(|(obj, _)| {
            //                     obj != &SequenceArgument::NestedResult(0, 0)
            //                         && obj != &SequenceArgument::NestedResult(0, 1)
            //                 });
            //             });
            //         let coin_a_type = MoveAbiSignatureToken::StructInstantiation(
            //             state.fuzz_state().get_struct(MoveAddress::two(), "balance", "Balance").unwrap(), vec![coin_a]
            //         );
            //         let coin_b_type = MoveAbiSignatureToken::StructInstantiation(
            //             state.fuzz_state().get_struct(MoveAddress::two(), "balance", "Balance").unwrap(), vec![coin_b]
            //         );

            //         if let ConstructResult::Ok(args, _, _) = try_construct_args(
            //             ptb.inputs.len() - 1,
            //             &vec![coin_a_type.clone()],
            //             &[],
            //             &BTreeMap::from([(0, coin_a.clone()), (1, coin_b.clone())]),
            //             &object_data,
            //             &self.db,
            //             state,
            //         ) {
            //             debug!("Found repay coin0: {:?}", args[0]);
            //             *provider.repay_split_coin0(ptb).unwrap() = args[0];
            //         }

            //         if let ConstructResult::Ok(args, _, _) = try_construct_args(
            //             ptb.inputs.len() - 1,
            //             &vec![coin_b_type.clone()],
            //             &[],
            //             &BTreeMap::from([(0, coin_a.clone()), (1, coin_b.clone())]),
            //             &object_data,
            //             &self.db,
            //             state,
            //         ) {
            //             debug!("Found repay coin1: {:?}", args[0]);
            //             *provider.repay_split_coin1(ptb).unwrap() = args[0];
            //         }
            //     }
            //     FlashProvider::Nemo { .. } => {}
            // }
        }
    }

    fn mutate_sequence(&mut self, state: &mut S, input: &mut I) -> MutationResult
    where
        I: MoveInput,
        S: HasRand + HasFuzzMetadata + HasFuzzEnv,
    {
        let ptb = input.sequence_mut();

        let inc = if ptb.commands.len() <= 3 {
            true
        } else {
            // Probability to add a movecall decreases as the sequence length increases
            state.rand_mut().next_float() < ADD_MOVECALL_PROB * 7.0 / ptb.commands.len() as f64
        };
        if !inc {
            let idx = state.rand_mut().below_or_zero(ptb.commands.len()) as u16;
            return self.remove_command(state.fuzz_state(), ptb, idx);
        }
        let mut selected_times = 0;
        let functions = state.fuzz_state().target_functions.clone();
        assert!(!functions.is_empty(), "No target functions available");
        let ptb_snapshot = ptb.clone();
        let mut result = MutationResult::Skipped;
        loop {
            if selected_times >= 10 {
                break;
            }
            selected_times += 1;
            let weights = functions
                .iter()
                .map(|f| {
                    state
                        .fuzz_state()
                        .function_scores
                        .get(f)
                        .cloned()
                        .unwrap_or(0)
                })
                .collect::<Vec<_>>();
            if weights.iter().all(|w| *w == 0) {
                return result;
            }
            for f in functions
                .iter()
                .zip(weights.iter())
                .filter(|(_f, w)| **w > INIT_FUNCTION_SCORE)
                .collect::<Vec<_>>()
            {
                debug!("function: {:?}, weight: {}", f.0, f.1);
            }
            let function = weighted_sample(&functions, &weights, state);
            let (idx, used_arguments) = if let Some(provider) = &self.flash {
                match provider {
                    FlashProvider::Cetus { .. } => (
                        ptb.commands.len() - 3,
                        vec![SequenceArgument::NestedResult(0, 2)],
                    ),
                    FlashProvider::Nemo { .. } => (
                        ptb.commands.len() - 1,
                        vec![SequenceArgument::NestedResult(1, 1)],
                    ),
                }
            } else {
                (ptb.commands.len(), vec![])
            };
            let template_cmds = ptb.commands.drain(idx..).collect::<Vec<_>>();
            if append_function(
                state,
                ptb,
                function,
                BTreeMap::new(),
                BTreeMap::new(),
                &used_arguments,
                false,
                0,
            )
            .is_none()
            {
                debug!(
                    "Failed to append function: {:?}, reverting to snapshot",
                    function
                );
                *ptb = ptb_snapshot.clone();
                continue;
            }
            ptb.commands.extend(template_cmds);
            self.finish(state, ptb);
            if ptb
                .commands
                .iter()
                .filter(|cmd| matches!(cmd, MoveSequenceCall::Call(mc) if mc.is_split()))
                .count()
                > 10
            {
                break;
            }
            let object_data = { ObjectData::from_ptb(ptb, state) };

            debug!(
                "loop iteration: {}, commands: {}, hot potatoes: {:?}",
                selected_times,
                ptb.commands.len(),
                object_data.hot_potatoes,
            );
            update_score(&object_data.hot_potatoes, state);
            if object_data.hot_potatoes.is_empty() {
                result = MutationResult::Mutated;
                break;
            }
        }

        if result == MutationResult::Skipped {
            *ptb = ptb_snapshot;
        }
        let object_data = { ObjectData::from_ptb(ptb, state) };
        update_score(&object_data.hot_potatoes, state);
        result
    }
}

impl<I, S> Mutator<I, S> for SequenceMutator<I, S>
where
    I: MoveInput,
    S: HasFuzzMetadata
        + HasRand
        + HasMetadata
        + HasFuzzEnv
        + HasExtraState<ExtraState = ExtraNonSerdeFuzzState<<S as HasFuzzEnv>::Env>>,
{
    fn mutate(&mut self, state: &mut S, input: &mut I) -> Result<MutationResult, libafl::Error> {
        let outcome = state.extra_state().global_outcome.as_ref();
        let extra = outcome.map(|o| &o.extra);

        if let Some(ex) = extra {
            input.update_magic_number(ex);
            debug!(
                "sequence mutator last stage: {:?}, stage outcome success: {}",
                ex.stage_idx, ex.success
            );
        }

        if let StageReplayAction::Replay {
            stage_idx,
            snapshot,
        } = self.stage.decide(
            state.fuzz_state().current_mutator,
            extra,
            ptb_fingerprint(input.sequence()),
        ) {
            *input.sequence_mut() = snapshot;
            let magic = state.rand_mut().below_or_zero(2) == 0;
            let res = mutate_arg(self, state, input, magic, &Some(stage_idx));
            if res == MutationResult::Mutated {
                self.stage.record_success(input.sequence());
                state.fuzz_state_mut().current_mutator = Some(MutatorKind::Sequence);
            }
            *input.outcome_mut() = None;
            return Ok(res);
        }

        let base = strip_generated(input.sequence(), state.fuzz_state());
        *input.sequence_mut() = base;

        let res = self.mutate_sequence(state, input);

        let magic = state.rand_mut().below_or_zero(2) == 0;
        mutate_arg(self, state, input, magic, &None);

        let decorated = apply_hooks(state, input.sequence());
        *input.sequence_mut() = decorated;
        *input.outcome_mut() = None;

        if res == MutationResult::Mutated {
            self.stage.record_success(input.sequence());
            state.fuzz_state_mut().current_mutator = Some(MutatorKind::Sequence);
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
