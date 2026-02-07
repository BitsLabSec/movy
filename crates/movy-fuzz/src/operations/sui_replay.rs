use libafl::{
    Evaluator, HasMetadata, StdFuzzer,
    corpus::{Corpus, InMemoryCorpus},
    events::SimpleEventManager,
    feedback_and_fast,
    feedbacks::{CrashFeedback, ExitKindFeedback, MaxMapPow2Feedback},
    monitors::SimpleMonitor,
    schedulers::QueueScheduler,
    state::{HasCorpus, HasSolutions, StdState},
};
use libafl_bolts::tuples::tuple_list;
use movy_replay::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo, ObjectStoreMintObject},
    env::SuiTestingEnv,
    exec::SuiExecutor,
    tracer::tree::TreeTracer,
};
use movy_sui::database::cache::ObjectSuiStoreCommit;
use movy_types::error::MovyError;
use sui_types::{
    effects::TransactionEffectsAPI,
    storage::{BackingPackageStore, BackingStore, ObjectStore},
};

use crate::{
    executor::SuiFuzzExecutor,
    input::MoveFuzzInput,
    meta::{FuzzMetadata, HasFuzzMetadata},
    operations::fuzz::{OkFeedback, code_observer},
    state::{HasFuzzEnv, SuperState},
    utils::AppendOutcomeFeedback,
};

pub fn sui_plain_replay_seed<T>(
    env: SuiTestingEnv<T>,
    meta: FuzzMetadata,
    seed: MoveFuzzInput,
    trace: bool,
) -> Result<(), MovyError>
where
    T: ObjectStore
        + BackingStore
        + BackingPackageStore
        + ObjectSuiStoreCommit
        + ObjectStoreMintObject
        + ObjectStoreInfo,
{
    let inner = env.into_inner();
    let executor = SuiExecutor::new(inner)?;
    let tracer = if trace { Some(TreeTracer::new()) } else { None };
    let out = executor.run_ptb_with_gas(
        seed.sequence.to_ptb()?,
        meta.epoch,
        meta.epoch_ms,
        meta.attacker.into(),
        meta.gas_id.into(),
        tracer,
    )?;
    log::info!("Replay status is {:?}", &out.results.effects.status());
    if let Some(tracer) = out.tracer {
        println!("Trace:\n{}", &tracer.take_inner().pprint());
    }

    Ok(())
}

pub fn sui_fuzz_replay_seed<T>(
    env: SuiTestingEnv<T>,
    meta: FuzzMetadata,
    seed: MoveFuzzInput,
) -> Result<(), MovyError>
where
    T: ObjectStoreCachedStore
        + ObjectStoreInfo
        + ObjectStore
        + ObjectSuiStoreCommit
        + BackingStore
        + BackingPackageStore
        + Clone
        + 'static,
{
    let code_observer = code_observer();
    let coverage_feedback = MaxMapPow2Feedback::with_name("code-fb", &code_observer);

    let mut corpus_feedback = feedback_and_fast!(
        ExitKindFeedback::<OkFeedback>::new(),
        AppendOutcomeFeedback {},
        coverage_feedback
    );
    let mut crash_feedback = feedback_and_fast!(CrashFeedback::new(), AppendOutcomeFeedback {});

    let corpus = InMemoryCorpus::new();
    let crashes = InMemoryCorpus::new();

    let state = StdState::new(
        meta.rand.clone(),
        corpus,
        crashes,
        &mut corpus_feedback,
        &mut crash_feedback,
    )?;
    let attacker = meta.attacker;

    let mut state = SuperState::new(state, env);

    state.add_metadata::<FuzzMetadata>(meta);

    let executor_inner = SuiExecutor::new(state.fuzz_env().inner().clone())?;
    let mut executor = SuiFuzzExecutor {
        executor: executor_inner,
        ob: tuple_list!(code_observer),
        attacker,
        oracles: super::sui_fuzz::oracles(false, false, false),
        epoch: state.fuzz_state().epoch,
        epoch_ms: state.fuzz_state().epoch_ms,
        ph: std::marker::PhantomData,
    };

    let sched = QueueScheduler::new();
    let mut fuzzer = StdFuzzer::new(sched, corpus_feedback, crash_feedback);
    let mut mgr = SimpleEventManager::new(SimpleMonitor::new(|s| log::info!("{}", s)));
    let id = fuzzer.add_input(&mut state, &mut executor, &mut mgr, seed.clone())?;

    log::info!("The input corpus id is {}", id);
    let case = if let Ok(case) = state.corpus().get(id) {
        log::info!("The seed was found in corpus");
        case
    } else {
        log::info!("The seed was found in solutions");
        state.solutions().get(id).expect("also not in corpus?!")
    };

    let input_borrow = case.borrow();
    let input = input_borrow.input().as_ref().unwrap();
    let outcome = input.outcome.as_ref().unwrap();
    if outcome != seed.outcome.as_ref().unwrap() {
        log::info!("We have different outcome");
        log::info!(
            "Previous outcome:\n{:?}\nCurrent outcome:\n{:?}",
            seed.outcome,
            input.outcome
        );
    } else {
        log::info!("The replayed outcome is:\n{:?}", input.outcome);
    }
    Ok(())
}
