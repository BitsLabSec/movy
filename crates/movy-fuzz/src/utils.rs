use itertools::Itertools;
use libafl::{
    HasMetadata,
    corpus::{Corpus, CorpusId, Testcase},
    feedbacks::{Feedback, StateInitializer},
};
use libafl_bolts::{Named, rands::StdRand};
use movy_types::input::{MoveAddress, MoveStructTag, MoveTypeTag};
use rand_libafl::RngCore;
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    hash::{DefaultHasher, Hash, Hasher},
    marker::PhantomData,
};

use crate::{
    input::MoveInput,
    state::{ExtraNonSerdeFuzzState, HasExtraState, HasFuzzEnv},
};

pub fn random_seed() -> u64 {
    rand_libafl::rng().next_u64()
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SuperRand(pub StdRand);

impl SuperRand {
    pub fn new(seed: u64) -> Self {
        Self(StdRand::with_seed(seed))
    }
}

// Not really but... we are fuzzing anyway
impl rand_core_libafl::CryptoRng for SuperRand {}
impl rand_core_sui::CryptoRng for SuperRand {}
impl fastcrypto::traits::AllowedRng for SuperRand {}

impl rand_core_libafl::RngCore for SuperRand {
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        rand_core_libafl::RngCore::fill_bytes(&mut self.0, dst)
    }

    fn next_u32(&mut self) -> u32 {
        rand_core_libafl::RngCore::next_u32(&mut self.0)
    }

    fn next_u64(&mut self) -> u64 {
        rand_core_libafl::RngCore::next_u64(&mut self.0)
    }
}

impl rand_core_sui::RngCore for SuperRand {
    fn fill_bytes(&mut self, dst: &mut [u8]) {
        rand_core_libafl::RngCore::fill_bytes(&mut self.0, dst)
    }

    fn next_u32(&mut self) -> u32 {
        rand_core_libafl::RngCore::next_u32(&mut self.0)
    }

    fn next_u64(&mut self) -> u64 {
        rand_core_libafl::RngCore::next_u64(&mut self.0)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_sui::Error> {
        rand_core_libafl::RngCore::fill_bytes(&mut self.0, dest);
        Ok(())
    }
}

impl libafl_bolts::rands::Rand for SuperRand {
    fn below(&mut self, upper_bound_excl: std::num::NonZeroUsize) -> usize {
        self.0.below(upper_bound_excl)
    }

    fn below_or_zero(&mut self, n: usize) -> usize {
        self.0.below_or_zero(n)
    }

    fn between(&mut self, lower_bound_incl: usize, upper_bound_incl: usize) -> usize {
        self.0.between(lower_bound_incl, upper_bound_incl)
    }

    fn choose<I>(&mut self, from: I) -> Option<I::Item>
    where
        I: IntoIterator,
    {
        self.0.choose(from)
    }

    fn coinflip(&mut self, success_prob: f64) -> bool {
        self.0.coinflip(success_prob)
    }

    fn next(&mut self) -> u64 {
        self.0.next()
    }

    fn next_float(&mut self) -> f64 {
        self.0.next_float()
    }
    fn set_seed(&mut self, seed: u64) {
        self.0.set_seed(seed);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SelectiveCorpus<C1, C2, I> {
    Corpus1(C1, PhantomData<I>),
    Corpus2(C2, PhantomData<I>),
}

macro_rules! select_corpus {
    ($sf: tt, $it: tt, $( $args: tt), *) => {
        match $sf {
            Self::Corpus1(c1, _) => c1.$it( $($args),* ),
            Self::Corpus2(c2, _) => c2.$it( $($args),* ),
        }
    };
}

impl<C1, C2, I> SelectiveCorpus<C1, C2, I>
where
    C1: Corpus<I>,
    C2: Corpus<I>,
{
    pub fn corpus1(c: C1) -> Self {
        Self::Corpus1(c, PhantomData)
    }

    pub fn corpus2(c: C2) -> Self {
        Self::Corpus2(c, PhantomData)
    }
}

impl<C1, C2, I> Corpus<I> for SelectiveCorpus<C1, C2, I>
where
    C1: Corpus<I>,
    C2: Corpus<I>,
{
    fn peek_free_id(&self) -> CorpusId {
        select_corpus!(self, peek_free_id,)
    }

    fn add(&mut self, testcase: Testcase<I>) -> Result<CorpusId, libafl::Error> {
        select_corpus!(self, add, testcase)
    }

    fn add_disabled(&mut self, testcase: Testcase<I>) -> Result<CorpusId, libafl::Error> {
        select_corpus!(self, add_disabled, testcase)
    }

    fn cloned_input_for_id(&self, idx: CorpusId) -> Result<I, libafl::Error>
    where
        I: Clone,
    {
        select_corpus!(self, cloned_input_for_id, idx)
    }

    fn count(&self) -> usize {
        select_corpus!(self, count,)
    }

    fn count_all(&self) -> usize {
        select_corpus!(self, count_all,)
    }

    fn count_disabled(&self) -> usize {
        select_corpus!(self, count_disabled,)
    }

    fn current(&self) -> &Option<CorpusId> {
        select_corpus!(self, current,)
    }

    fn current_mut(&mut self) -> &mut Option<CorpusId> {
        select_corpus!(self, current_mut,)
    }

    fn first(&self) -> Option<CorpusId> {
        select_corpus!(self, first,)
    }

    fn get(&self, id: CorpusId) -> Result<&RefCell<Testcase<I>>, libafl::Error> {
        select_corpus!(self, get, id)
    }

    fn get_from_all(&self, id: CorpusId) -> Result<&RefCell<Testcase<I>>, libafl::Error> {
        select_corpus!(self, get_from_all, id)
    }

    fn is_empty(&self) -> bool {
        select_corpus!(self, is_empty,)
    }

    fn last(&self) -> Option<CorpusId> {
        select_corpus!(self, last,)
    }

    fn load_input_into(&self, testcase: &mut Testcase<I>) -> Result<(), libafl::Error> {
        select_corpus!(self, load_input_into, testcase)
    }

    fn next(&self, id: CorpusId) -> Option<CorpusId> {
        select_corpus!(self, next, id)
    }

    fn nth(&self, nth: usize) -> CorpusId {
        select_corpus!(self, nth, nth)
    }

    fn nth_from_all(&self, nth: usize) -> CorpusId {
        select_corpus!(self, nth_from_all, nth)
    }

    fn prev(&self, id: CorpusId) -> Option<CorpusId> {
        select_corpus!(self, prev, id)
    }

    fn remove(&mut self, id: CorpusId) -> Result<Testcase<I>, libafl::Error> {
        select_corpus!(self, remove, id)
    }

    fn replace(
        &mut self,
        idx: CorpusId,
        testcase: Testcase<I>,
    ) -> Result<Testcase<I>, libafl::Error> {
        select_corpus!(self, replace, idx, testcase)
    }

    fn store_input_from(&self, testcase: &Testcase<I>) -> Result<(), libafl::Error> {
        select_corpus!(self, store_input_from, testcase)
    }
}

// pub fn pprint_object(object: &Object) -> String {
//     let data = match &object.data {
//         Data::Move(mv) => {
//             format!("Object(ty={})", mv.type_().to_canonical_string(true))
//         }
//         Data::Package(pk) => {
//             format!(
//                 "Package(modules=[{}])",
//                 pk.serialized_module_map().iter().map(|t| t.0).join(",")
//             )
//         }
//     };
//     format!(
//         "(ID={}, version={}, owner={} data={})",
//         object.id(),
//         object.version(),
//         object.owner(),
//         data
//     )
// }

// pub fn pprint_structtag(tag: &StructTag) -> String {
//     format!(
//         "{{{}}}",
//         tag.type_params
//             .iter()
//             .map(|t| t.to_canonical_string(true))
//             .join(", ")
//     )
// }

// pub fn pprint_event_plain(event: &Event) -> String {
//     format!(
//         "Event(package={}, sender={}, ty={}, contents={})",
//         &event.package_id,
//         &event.sender,
//         &event.type_,
//         const_hex::encode(&event.contents)
//     )
// }

// pub fn pprint_event_decode(event: &Event, meta: &FuzzMetadata) -> String {
//     let e = may_be_logging(event)
//         .map(|t| t.to_string())
//         .or_else(|| may_be_oracle(event).map(|t| t.to_string()))
//         .unwrap_or_else(|| match meta.try_decode_event(event) {
//             Ok(v) => match v {
//                 Some(v) => serde_json::to_string(&v.1).expect("value can not to_string?!"),
//                 None => "(No such event defination?)".to_string(),
//             },
//             Err(e) => format!("(Unable to decode due to {})", e),
//         });
//     format!(
//         "Event(package={}, sender={}, ty={}, contents={})",
//         &event.package_id, &event.sender, &event.type_, e
//     )
// }

// pub fn pprint_event(event: &Event, meta: Option<&FuzzMetadata>) -> String {
//     if let Some(meta) = meta {
//         pprint_event_decode(event, meta)
//     } else {
//         pprint_event_plain(event)
//     }
// }

pub fn value_to_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Number(num) => num.as_u64(),
        serde_json::Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    }
}

pub fn change_type_address(
    s: &mut MoveStructTag,
    old_addr: MoveAddress,
    new_addr: MoveAddress,
) -> bool {
    let mut changed = false;
    if s.address == old_addr {
        s.address = new_addr;
        changed = true;
    }
    changed |= s.tys.iter_mut().any(|ty| match ty {
        MoveTypeTag::Struct(s) => change_type_address(s, old_addr, new_addr),
        MoveTypeTag::Vector(v) => match v.as_mut() {
            MoveTypeTag::Struct(s) => change_type_address(s, old_addr, new_addr),
            _ => false,
        },
        _ => false,
    });
    changed
}

pub fn hash_to_u64(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

pub struct AppendOutcomeFeedback {}

impl Named for AppendOutcomeFeedback {
    fn name(&self) -> &std::borrow::Cow<'static, str> {
        &std::borrow::Cow::Borrowed("AppendOutcomeFeedback")
    }
}

impl<S> StateInitializer<S> for AppendOutcomeFeedback {
    fn init_state(&mut self, _state: &mut S) -> Result<(), libafl::Error> {
        Ok(())
    }
}

impl<EM, I, OT, S> Feedback<EM, I, OT, S> for AppendOutcomeFeedback
where
    I: MoveInput,
    S: HasMetadata
        + HasFuzzEnv
        + HasExtraState<ExtraState = ExtraNonSerdeFuzzState<<S as HasFuzzEnv>::Env>>,
{
    fn is_interesting(
        &mut self,
        _state: &mut S,
        _manager: &mut EM,
        _input: &I,
        _observers: &OT,
        _exit_kind: &libafl::executors::ExitKind,
    ) -> Result<bool, libafl::Error> {
        Ok(true)
    }

    fn append_metadata(
        &mut self,
        state: &mut S,
        _manager: &mut EM,
        _observers: &OT,
        _testcase: &mut Testcase<I>,
    ) -> Result<(), libafl::Error> {
        let outcome = std::mem::take(&mut state.extra_state_mut().global_outcome);

        if let Some(outcome) = outcome {
            let input = _testcase.input_mut().as_mut().unwrap();
            let execution_outcome = outcome.exec;
            *input.outcome_mut() = Some(execution_outcome);
            *input.display_mut() = Some(input.format());
        }

        #[cfg(feature = "input_display")]
        {
            use crate::meta::HasFuzzMetadata;

            // TODO: Format with outcome
            *_testcase.input_mut().as_mut().unwrap().display_mut() = Some(
                _testcase
                    .input()
                    .as_ref()
                    .unwrap()
                    .format_with_meta(Some(state.fuzz_state())),
            );
        }

        Ok(())
    }
}
