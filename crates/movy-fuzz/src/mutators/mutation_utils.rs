use alloy_primitives::{U128, U256};
use libafl::{
    Error,
    inputs::HasMutatorBytes,
    mutators::{DwordInterestingMutator, HavocScheduledMutator, MutationResult, Mutator},
    state::HasRand,
};
use libafl_bolts::{HasLen, Named, rands::Rand, tuples::tuple_list};
use movy_types::input::{InputArgument, MoveTypeTag};
use std::collections::BTreeSet;

use crate::{r#const::MAX_STACK_POW, meta::HasCaller};

/// [`MagicNumberMutator`] is a mutator that mutates the input to a constant
/// in the contract
///
/// We discover that sometimes directly setting the bytes to the constants allow
/// us to increase test coverage.
#[derive(Default)]
pub struct MagicNumberMutator {
    magic_number_pool: Vec<Vec<u8>>,
}

impl Named for MagicNumberMutator {
    fn name(&self) -> &std::borrow::Cow<'static, str> {
        &std::borrow::Cow::Borrowed("constant_hinted_mutator")
    }
}

impl MagicNumberMutator {
    pub fn new(magic_number_pool: BTreeSet<Vec<u8>>) -> Self {
        Self {
            magic_number_pool: magic_number_pool.into_iter().collect(),
        }
    }
}

impl<I, S> Mutator<I, S> for MagicNumberMutator
where
    S: HasRand,
    I: HasMutatorBytes,
{
    /// Mutate the input to a magic number in the contract
    /// This always entirely overwrites the input (unless it skips mutation)
    fn mutate(&mut self, state: &mut S, input: &mut I) -> Result<MutationResult, Error> {
        if self.magic_number_pool.is_empty() {
            return Ok(MutationResult::Skipped);
        }
        let input_bytes = input.mutator_bytes_mut();
        let input_len = input_bytes.len();
        let fit_pool: Vec<_> = self
            .magic_number_pool
            .iter()
            .filter(|x| x.len() == input_len)
            .collect();
        if fit_pool.is_empty() {
            return Ok(MutationResult::Skipped);
        }
        let magic_number = fit_pool[state.rand_mut().below_or_zero(fit_pool.len())].clone();

        let magic_number_len = magic_number.len();

        if input_len < magic_number_len {
            input_bytes.copy_from_slice(&magic_number[0..input_len]);
        } else {
            input_bytes.copy_from_slice(
                &[vec![0; input_len - magic_number_len], magic_number.clone()].concat(),
            );
        }

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

pub struct MutableValue {
    pub value: InputArgument,
    pub bytes: Vec<u8>,
}

impl HasLen for MutableValue {
    fn len(&self) -> usize {
        self.bytes.len()
    }
}

impl HasMutatorBytes for MutableValue {
    fn mutator_bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn mutator_bytes_mut(&mut self) -> &mut [u8] {
        &mut self.bytes
    }
}

fn sync(value: &InputArgument) -> Vec<u8> {
    match value {
        InputArgument::U128(v) => v.to_le_bytes::<16>().to_vec(),
        InputArgument::U256(v) => v.to_le_bytes::<32>().to_vec(),
        InputArgument::U64(v) => v.to_le_bytes().to_vec(),
        InputArgument::U32(v) => v.to_le_bytes().to_vec(),
        InputArgument::U16(v) => v.to_le_bytes().to_vec(),
        InputArgument::U8(v) => v.to_le_bytes().to_vec(),
        InputArgument::Vector(_, vs) => vs.iter().flat_map(sync).collect(),
        _ => unreachable!("type: {:?}", value),
    }
}

fn commit(bytes: &[u8], value: &InputArgument) -> InputArgument {
    match value {
        InputArgument::U128(_) => {
            InputArgument::U128(U128::from_le_bytes::<16>(bytes.try_into().unwrap()))
        }
        InputArgument::U64(_) => InputArgument::U64(u64::from_le_bytes(bytes.try_into().unwrap())),
        InputArgument::U32(_) => InputArgument::U32(u32::from_le_bytes(bytes.try_into().unwrap())),
        InputArgument::U16(_) => InputArgument::U16(u16::from_le_bytes(bytes.try_into().unwrap())),
        InputArgument::U8(_) => InputArgument::U8(u8::from_le_bytes(bytes.try_into().unwrap())),
        InputArgument::U256(_) => {
            InputArgument::U256(U256::from_le_bytes::<32>(bytes.try_into().unwrap()))
        }
        InputArgument::Vector(t, vs) => InputArgument::Vector(
            t.clone(),
            vs.iter()
                .enumerate()
                .map(|(i, _)| {
                    let chunk_size = match t {
                        MoveTypeTag::U8 => 1,
                        MoveTypeTag::U16 => 2,
                        MoveTypeTag::U32 => 4,
                        MoveTypeTag::U64 => 8,
                        MoveTypeTag::U128 => 16,
                        MoveTypeTag::U256 => 32,
                        _ => unreachable!(),
                    };
                    let start = i * chunk_size;
                    let end = start + chunk_size;
                    commit(&bytes[start..end], &vs[i])
                })
                .collect(),
        ),
        _ => unreachable!(),
    }
}

impl MutableValue {
    pub fn new(value: InputArgument) -> Self {
        let bytes = vec![];

        MutableValue { value, bytes }
    }

    /// Mutator that mutates the `CONSTANT SIZE` input bytes (e.g., uint256) in
    /// various ways provided by [`libafl::mutators`]. It also uses the
    /// [`ConstantHintedMutator`] and [`VMStateHintedMutator`]
    fn mutate_by(
        &mut self,
        state: &mut impl HasRand,
        magic_number_pool: &BTreeSet<Vec<u8>>,
        split: bool,
    ) -> MutationResult {
        let mut bytes = sync(&self.value);

        let mutations = tuple_list!(MagicNumberMutator::new(magic_number_pool.clone()),);

        let mut mutator = HavocScheduledMutator::with_max_stack_pow(mutations, MAX_STACK_POW);
        let mut res = mutator.mutate(state, self).unwrap();

        if bytes.len() == 4 {
            if state.rand_mut().below_or_zero(10) > 0 {
                bytes = (state.rand_mut().below_or_zero(443636) as u32)
                    .to_le_bytes()
                    .to_vec();
                res = MutationResult::Mutated;
            }
            // let mut i = 0x10000u32;
            // if state.rand_mut().below_or_zero(2) == 0 {
            //     i -= 0x1000;
            //     bytes = i.to_le_bytes().to_vec();
            // } else {
            //     i += 0x1000;
            //     bytes = i.to_le_bytes().to_vec();
            // }
        } else if bytes.len() == 8 {
            if state.rand_mut().below_or_zero(10) > 5 {
                bytes = (1u64 << (state.rand_mut().below_or_zero(64) as u32))
                    .to_le_bytes()
                    .to_vec();
                res = MutationResult::Mutated;
            }
        } else if bytes.len() == 16 {
            if state.rand_mut().below_or_zero(10) > 0 {
                bytes = (1u128 << (state.rand_mut().below_or_zero(128) as u32))
                    .to_le_bytes()
                    .to_vec();
                res = MutationResult::Mutated;
            }
        } else if split && magic_number_pool.is_empty() {
            bytes = 0u64.to_le_bytes().to_vec();
            res = MutationResult::Mutated;
        } else {
            let mutations = tuple_list!(DwordInterestingMutator::new());

            let mut mutator = HavocScheduledMutator::with_max_stack_pow(mutations, MAX_STACK_POW);
            res = mutator.mutate(state, self).unwrap();
        }

        self.value = commit(&bytes, &self.value);
        res
    }

    pub fn mutate<S>(
        &mut self,
        state: &mut S,
        magic_number_pool: &BTreeSet<Vec<u8>>,
        split: bool,
    ) -> MutationResult
    where
        S: HasRand + HasCaller,
    {
        macro_rules! mutate_u {
            ($ty: ty, $v: expr) => {{
                let orig = *$v;
                while *$v == orig {
                    *$v = state.rand_mut().below_or_zero(<$ty>::MAX as usize) as $ty;
                    // debug!("mutate_u: {} {}", $v, orig);
                }
                MutationResult::Mutated
            }};
        }

        match self.value {
            // value level mutation
            InputArgument::Bool(ref mut v) => {
                *v = state.rand_mut().below_or_zero(2) == 1;
                return MutationResult::Mutated;
            }
            InputArgument::U8(ref mut v) => {
                return mutate_u!(u8, v);
            }
            InputArgument::U16(_)
            | InputArgument::U32(_)
            | InputArgument::U64(_)
            | InputArgument::U128(_)
            | InputArgument::U256(_) => {
                return self.mutate_by(state, magic_number_pool, split);
            }
            InputArgument::Address(ref mut v) => {
                if state.rand_mut().below_or_zero(2) == 1 {
                    *v = state.get_rand_address();
                } else {
                    *v = state.get_rand_caller();
                }
                return MutationResult::Mutated;
            }
            _ => {}
        }

        MutationResult::Skipped
    }

    pub fn sample_magic_number<S>(
        &mut self,
        state: &mut S,
        magic_number_pool: &BTreeSet<Vec<u8>>,
    ) -> MutationResult
    where
        S: HasRand + HasCaller,
    {
        match self.value {
            InputArgument::U8(_)
            | InputArgument::U16(_)
            | InputArgument::U32(_)
            | InputArgument::U64(_)
            | InputArgument::U128(_)
            | InputArgument::U256(_)
            | InputArgument::Vector(MoveTypeTag::U8, _)
            | InputArgument::Vector(MoveTypeTag::U16, _)
            | InputArgument::Vector(MoveTypeTag::U32, _)
            | InputArgument::Vector(MoveTypeTag::U64, _)
            | InputArgument::Vector(MoveTypeTag::U128, _)
            | InputArgument::Vector(MoveTypeTag::U256, _) => {
                let mut bytes = sync(&self.value);
                let fit_values = magic_number_pool
                    .iter()
                    .filter(|num| num.len() == bytes.len())
                    .collect::<Vec<_>>();
                if fit_values.is_empty() {
                    return MutationResult::Skipped;
                }
                bytes = state.rand_mut().choose(fit_values).unwrap().clone();
                self.value = commit(&bytes, &self.value);
                MutationResult::Mutated
            }
            _ => MutationResult::Skipped,
        }
    }
}
