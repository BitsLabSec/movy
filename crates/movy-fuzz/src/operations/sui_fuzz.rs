use std::{num::NonZero, path::PathBuf, time::Duration};

use crate::executor::SuiFuzzExecutor;
use crate::input::MoveFuzzInput;
use crate::meta::{FuzzMetadata, HasFuzzMetadata};
use crate::mutators::arg::ArgMutator;
use crate::mutators::sequence::SequenceMutator;
use crate::operations::fuzz::{OkFeedback, code_observer};
use crate::oracles::sui::{
    BoolJudgementOracle, InfiniteLoopOracle, OverflowOracle, PrecisionLossOracle, ProceedsOracle,
    TypeConversionOracle, TypedBugOracle,
};
use crate::sched::MoveFuzzInputScore;
use crate::state::{ExtraNonSerdeFuzzState, HasExtraState, HasFuzzEnv, SuperState};
use crate::utils::{AppendOutcomeFeedback, SelectiveCorpus};
use libafl::{
    Evaluator, Fuzzer, HasMetadata, StdFuzzer,
    corpus::{InMemoryCorpus, InMemoryOnDiskCorpus},
    events::{ProgressReporter, SimpleEventManager},
    feedback_and_fast,
    feedbacks::{CrashFeedback, ExitKindFeedback, MaxMapPow2Feedback},
    monitors::SimpleMonitor,
    schedulers::WeightedScheduler,
    stages::{CalibrationStage, StdMutationalStage},
    state::StdState,
};
use libafl_bolts::tuples::tuple_list;
use log::{info, warn};
use movy_replay::db::{ObjectStoreCachedStore, ObjectStoreInfo};
use movy_replay::env::SuiTestingEnv;
use movy_replay::exec::SuiExecutor;
use movy_replay::tracer::oracle::{CouldDisabledOralce, SuiGeneralOracle};
use movy_sui::database::cache::{CachedStore, ObjectSuiStoreCommit};
use movy_types::error::MovyError;
use sui_types::storage::{BackingPackageStore, BackingStore, ObjectStore};

pub fn oracles<T, S, E>(
    typed_bug_abort: bool,
    disable_profit_oracle: bool,
    disable_defects_oracle: bool,
) -> impl for<'a> SuiGeneralOracle<CachedStore<&'a T>, S>
where
    T: 'static + ObjectStore,
    S: HasMetadata + HasExtraState<ExtraState = ExtraNonSerdeFuzzState<E>> + HasFuzzMetadata,
{
    tuple_list!(
        CouldDisabledOralce::new(BoolJudgementOracle, disable_defects_oracle),
        CouldDisabledOralce::new(InfiniteLoopOracle::default(), disable_defects_oracle),
        CouldDisabledOralce::new(PrecisionLossOracle, disable_defects_oracle),
        CouldDisabledOralce::new(TypeConversionOracle, disable_defects_oracle),
        CouldDisabledOralce::new(OverflowOracle, disable_defects_oracle),
        CouldDisabledOralce::new(ProceedsOracle::default(), disable_profit_oracle),
        CouldDisabledOralce::new(TypedBugOracle::new(typed_bug_abort), disable_defects_oracle),
    )
}

fn fuzz_impl<T>(
    meta: FuzzMetadata,
    env: SuiTestingEnv<T>,
    output: &Option<PathBuf>,
    time_limit: Option<u64>,
    typed_bug_abort: bool,
    disable_profit_oracle: bool,
    disable_defects_oracle: bool,
) -> Result<(), MovyError>
where
    T: ObjectStoreCachedStore
        + ObjectStoreInfo
        + ObjectStore
        + ObjectSuiStoreCommit
        + BackingStore
        + BackingPackageStore
        + BackingPackageStore
        + Clone
        + 'static,
{
    let code_observer = code_observer();
    let coverage_feedback = MaxMapPow2Feedback::with_name("code-fb", &code_observer);

    let calib = CalibrationStage::new(&coverage_feedback);
    let mut corpus_feedback = feedback_and_fast!(
        ExitKindFeedback::<OkFeedback>::new(),
        AppendOutcomeFeedback {},
        coverage_feedback
    );
    let mut crash_feedback = feedback_and_fast!(
        CrashFeedback::new(),
        AppendOutcomeFeedback {},
        MaxMapPow2Feedback::with_name("crash-fb", &code_observer)
    );

    let corpus = if let Some(output) = output {
        let corpus = output.join("queue");
        std::fs::create_dir_all(&corpus)?;
        SelectiveCorpus::corpus1(InMemoryOnDiskCorpus::<MoveFuzzInput>::new(corpus)?)
    } else {
        SelectiveCorpus::corpus2(InMemoryCorpus::<MoveFuzzInput>::new())
    };

    let crashes = if let Some(output) = output {
        let crash = output.join("crashes");
        std::fs::create_dir_all(&crash)?;
        SelectiveCorpus::corpus1(InMemoryOnDiskCorpus::new(crash)?)
    } else {
        SelectiveCorpus::corpus2(InMemoryCorpus::new())
    };

    let state = StdState::new(
        meta.rand.clone(),
        corpus,
        crashes,
        &mut corpus_feedback,
        &mut crash_feedback,
    )?;
    let attacker = meta.attacker;

    let mut state = SuperState::new(state, env);

    info!("target functions: {:?}", meta.target_functions);
    info!(
        "module address to package: {:?}",
        meta.module_address_to_package
    );
    state.add_metadata::<FuzzMetadata>(meta);

    let executor_inner = SuiExecutor::new(state.fuzz_env().inner().clone())?;

    let sched: WeightedScheduler<_, MoveFuzzInputScore, _> =
        WeightedScheduler::new(&mut state, &code_observer);
    let mut executor = SuiFuzzExecutor {
        executor: executor_inner,
        ob: tuple_list!(code_observer),
        attacker,
        oracles: oracles(
            typed_bug_abort,
            disable_profit_oracle,
            disable_defects_oracle,
        ),
        epoch: state.fuzz_state().epoch,
        epoch_ms: state.fuzz_state().epoch_ms,
        ph: std::marker::PhantomData,
    };

    let mut stages = tuple_list!(
        calib,
        StdMutationalStage::with_max_iterations(SequenceMutator::new(), NonZero::new(256).unwrap()),
        StdMutationalStage::with_max_iterations(ArgMutator::new(), NonZero::new(256).unwrap()),
    );

    let mut fuzzer = StdFuzzer::new(sched, corpus_feedback, crash_feedback);
    let mut mgr = SimpleEventManager::new(SimpleMonitor::new(|s| info!("{}", s)));

    info!("Adding initial input...");
    let initial_input = MoveFuzzInput::new();
    fuzzer.add_input(&mut state, &mut executor, &mut mgr, initial_input)?;

    // if let Some(flash) = &self.flash {
    //     let flash_wrapper = FlashWrapper::from_str_with_store(flash, &db)?;
    //     let mut flash_seed = SuiFuzzInput::flash(flash_wrapper.clone())?;

    //     tokio::task::block_in_place(|| {
    //         tokio::runtime::Handle::current().block_on(async {
    //             state
    //                 .fuzz_state_mut()
    //                 .analyze_object_types(&rpc, &executor.db)
    //                 .await?;
    //             match flash_wrapper.provider {
    //                 FlashProvider::Cetus { package, .. } => {
    //                     state
    //                         .fuzz_state_mut()
    //                         .add_single_package(&rpc, package, &executor.db)
    //                         .await?;
    //                 }
    //                 FlashProvider::Nemo { package, .. } => {
    //                     state
    //                         .fuzz_state_mut()
    //                         .add_single_package(&rpc, package, &executor.db)
    //                         .await?;
    //                 }
    //             }

    //             Ok::<_, MovyError>(())
    //         })
    //     })?;
    //     executor.disable_oracles();
    //     process_key_store(&mut flash_seed.ptb, &state, &executor.db);

    //     info!("We will add an input:\n{}", &flash_seed);
    //     let (result, _corpus_id) =
    //         fuzzer.evaluate_input(&mut state, &mut executor, &mut mgr, &flash_seed)?;

    //     if !result.is_corpus() || result.is_solution() {
    //         return Err(eyre!("flash input is unexpected with {:?}", result).into());
    //     }
    //     remove_process_key_store(&mut flash_seed.ptb);
    //     executor.enable_oracles();
    // }

    #[cfg(feature = "pprof")]
    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(1000)
        .blocklist(&["libc", "libgcc", "pthread", "vdso"])
        .build()
        .unwrap();

    let start = std::time::SystemTime::now();
    let mut cycle = 1usize;
    loop {
        if let Some(limit) = time_limit {
            let current = std::time::SystemTime::now();

            let elapsed = current.duration_since(start).expect("non mono clock?!");
            if elapsed > Duration::from_secs(limit) {
                break;
            }
        }

        if let Err(e) = fuzzer.fuzz_one(&mut stages, &mut executor, &mut state, &mut mgr) {
            warn!("Getting fuzz error: {:?}", e);
            break;
        }

        // Clear per-round execution outcome to avoid leaking stage indices into the next round.
        state.extra_state_mut().global_outcome = None;

        info!("Cycle {} done", cycle);
        cycle += 1;
        mgr.report_progress(&mut state)?;
    }

    #[cfg(feature = "pprof")]
    {
        let report = guard.report().build().expect("generate report");
        let file = std::fs::File::create("flamegraph.svg").unwrap();
        report.flamegraph(file).unwrap();
    }

    Ok(())
}

pub fn fuzz(
    meta: FuzzMetadata,
    env: SuiTestingEnv<
        impl ObjectStoreCachedStore
        + ObjectStoreInfo
        + ObjectSuiStoreCommit
        + BackingStore
        + BackingPackageStore
        + Clone
        + 'static,
    >,
    output: &Option<PathBuf>,
    time_limit: Option<u64>,
    typed_bug_abort: bool,
    disable_profit_oracle: bool,
    disable_defects_oracle: bool,
) -> Result<(), MovyError> {
    fuzz_impl(
        meta,
        env,
        output,
        time_limit,
        typed_bug_abort,
        disable_profit_oracle,
        disable_defects_oracle,
    )?;
    Ok(())
}
