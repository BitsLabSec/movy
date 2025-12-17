use crate::input::MoveInput;
use libafl::{
    HasMetadata,
    corpus::{SchedulerTestcaseMetadata, Testcase},
    feedbacks::MapIndexesMetadata,
    schedulers::{SchedulerMetadata, TestcaseScore, powersched::BaseSchedule},
    state::HasCorpus,
};
use libafl_bolts::HasRefCnt;
use movy_types::input::MoveSequenceCall;

/// The weight for each corpus entry
/// This result is used for corpus scheduling
#[derive(Debug, Clone)]
pub struct MoveFuzzInputScore {}

impl<I, S> TestcaseScore<I, S> for MoveFuzzInputScore
where
    S: HasCorpus<I> + HasMetadata,
    I: MoveInput,
{
    fn compute(state: &S, entry: &mut Testcase<I>) -> Result<f64, libafl::Error> {
        let mut weight = 1.0;
        let psmeta = state.metadata::<SchedulerMetadata>()?;

        let tcmeta = entry.metadata::<SchedulerTestcaseMetadata>()?;
        if entry.scheduled_count() == 0 || psmeta.cycles() == 0 {
            return Ok(weight * 5.0f64);
        }

        let q_exec_us = entry
            .exec_time()
            .ok_or_else(|| libafl::Error::key_not_found("exec_time not set".to_string()))?
            .as_nanos() as f64;

        let avg_exec_us = psmeta.exec_time().as_nanos() as f64 / psmeta.cycles() as f64;
        let avg_bitmap_size = psmeta.bitmap_size_log() / psmeta.bitmap_entries() as f64;

        let q_bitmap_size = tcmeta.bitmap_size() as f64;

        if let Some(ps) = psmeta.strat() {
            match ps.base() {
                BaseSchedule::FAST | BaseSchedule::COE | BaseSchedule::LIN | BaseSchedule::QUAD => {
                    let hits = psmeta.n_fuzz()[tcmeta.n_fuzz_entry()];
                    if hits > 0 {
                        weight /= f64::from(hits).log10() + 1.0;
                    }
                }
                _ => (),
            }
        }

        weight *= avg_exec_us / q_exec_us;
        weight *= if avg_bitmap_size.abs() < 0.0000001 {
            1.0
        } else {
            q_bitmap_size.log2().max(1.0) / avg_bitmap_size
        };

        let tc_ref = match entry.metadata_map().get::<MapIndexesMetadata>() {
            Some(meta) => meta.refcnt() as f64,
            None => 0.0,
        };

        weight *= 1.0 + tc_ref;

        let mcs = entry
            .input()
            .as_ref()
            .unwrap()
            .sequence()
            .commands
            .iter()
            .filter(|t| match t {
                MoveSequenceCall::Call(_) => true,
                _ => false,
            })
            .count();

        if mcs >= 2 {
            weight /= 2.0f64
        } else if mcs >= 4 {
            weight /= 3.0f64
        } else if mcs >= 8 {
            weight /= 4.0f64
        }

        if !weight.is_normal() {
            return Err(libafl::Error::illegal_state(
                format!(
                    "abnormal weight {}, psmeta {:?}, tcmeta {:?}",
                    weight, psmeta, tcmeta
                )
                .as_str(),
            ));
        }

        Ok(weight)
    }
}
