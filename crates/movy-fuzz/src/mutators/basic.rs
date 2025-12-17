use std::marker::PhantomData;

use libafl::mutators::{MutationResult, Mutator};
use libafl_bolts::Named;

use crate::{input::MoveInput, meta::HasFuzzMetadata};

pub struct BasicMuator<I, S> {
    pub ph: PhantomData<(I, S)>,
}

impl<I, S> Default for BasicMuator<I, S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<I, S> BasicMuator<I, S> {
    pub fn new() -> Self {
        Self { ph: PhantomData }
    }
}

impl<I, S> Named for BasicMuator<I, S> {
    fn name(&self) -> &std::borrow::Cow<'static, str> {
        &std::borrow::Cow::Borrowed("basicmutator")
    }
}

impl<I, S> Mutator<I, S> for BasicMuator<I, S>
where
    I: MoveInput,
    S: HasFuzzMetadata,
{
    fn mutate(&mut self, _state: &mut S, _input: &mut I) -> Result<MutationResult, libafl::Error> {
        Ok(MutationResult::Mutated)
    }

    fn post_exec(
        &mut self,
        _state: &mut S,
        _new_corpus_id: Option<libafl::corpus::CorpusId>,
    ) -> Result<(), libafl::Error> {
        Ok(())
    }
}
