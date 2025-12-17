mod bool_judgement;
mod common;
mod generate_bytecode;
mod infinite_loop;
mod precision_loss;
mod type_conversion;
mod unchecked_return;
mod unused_const;
mod unused_private_fun;
mod unused_struct;

use movy_replay::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    env::SuiTestingEnv,
};
use movy_types::{error::MovyError, input::MoveAddress, oracle::OracleFinding};
use sui_types::storage::ObjectStore;

pub use common::ModuleAnalysis;
pub use generate_bytecode::FunctionInfo;

/// Run all static analyses that were originally implemented as once-per-world oracles.
/// Returns structured findings; the caller can decide how to surface them.
pub async fn run_all<T>(
    env: &SuiTestingEnv<T>,
    target_packages: &Vec<MoveAddress>,
) -> Result<Vec<OracleFinding>, MovyError>
where
    T: ObjectStore + ObjectStoreInfo + ObjectStoreCachedStore,
{
    let modules = common::load_target_modules(env, target_packages).await?;

    let mut reports = Vec::new();
    reports.extend(bool_judgement::analyze(&modules));
    reports.extend(infinite_loop::analyze(&modules));
    reports.extend(precision_loss::analyze(&modules));
    reports.extend(type_conversion::analyze(&modules));
    reports.extend(unchecked_return::analyze(&modules));
    reports.extend(unused_const::analyze(env, target_packages).await?);
    reports.extend(unused_private_fun::analyze(env, target_packages).await?);
    reports.extend(unused_struct::analyze(env, target_packages).await?);

    Ok(reports)
}
