use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    str::FromStr,
};

use color_eyre::eyre::eyre;
use itertools::Itertools;
use move_core_types::account_address::AccountAddress;
use movy_sui::{
    compile::{SuiCompiledPackage, mock_module_address},
    database::{cache::ObjectSuiStoreCommit, graphql::GraphQlDatabase},
    rpc::graphql::{GraphQlClient, OwnerKind},
};
use movy_types::{
    abi::{MOVY_INIT, MovePackageAbi},
    error::MovyError,
    input::{MoveAddress, MoveStructTag},
};
use sui_types::{
    Identifier,
    base_types::{ObjectID, SequenceNumber},
    digests::TransactionDigest,
    effects::TransactionEffectsAPI,
    move_package::{MovePackage, UpgradeCap},
    object::{Data, Object},
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    storage::{BackingPackageStore, BackingStore, ObjectStore, WriteKind},
    transaction::{
        Argument, Command, ObjectArg, TransactionData, TransactionDataAPI, TransactionKind,
    },
};

use crate::{
    db::{ObjectStoreCachedStore, ObjectStoreInfo},
    exec::SuiExecutor,
    tracer::{NopTracer, tree::TreeTracer},
};

pub struct SuiTestingEnv<T> {
    db: T,
}

#[derive(Debug, Clone)]
struct OnchainUpgradeStep {
    chain_from: ObjectID,
    chain_to: ObjectID,
    source_tx: TransactionDigest,
    source_checkpoint: u64,
    modules: Vec<Vec<u8>>,
    dependencies: Vec<ObjectID>,
    use_local_modules: bool,
}

fn extract_upgrade_command(
    tx_data: &TransactionData,
) -> Option<(Vec<Vec<u8>>, Vec<ObjectID>, ObjectID)> {
    let TransactionKind::ProgrammableTransaction(ptb) = tx_data.kind() else {
        return None;
    };
    for command in &ptb.commands {
        if let Command::Upgrade(modules, deps, package, _ticket) = command {
            return Some((modules.clone(), deps.clone(), *package));
        }
    }
    None
}

impl<T> SuiTestingEnv<T> {
    pub fn inner(&self) -> &T {
        &self.db
    }

    pub fn inner_mut(&mut self) -> &mut T {
        &mut self.db
    }
    pub fn into_inner(self) -> T {
        self.db
    }
}

impl<
    T: ObjectStoreCachedStore
        + ObjectStoreInfo
        + ObjectStore
        + ObjectSuiStoreCommit
        + BackingStore
        + BackingPackageStore
        + Clone
        + 'static,
> SuiTestingEnv<T>
{
    pub fn new(db: T) -> Self {
        Self { db }
    }

    pub fn install_movy(&self) -> Result<(), MovyError> {
        let movy = movy_sui_stds::movy();
        tracing::info!("Installing movy to {}", movy.package_id);
        let (modules, deps) = movy.into_deployment();
        let movy_package = Object::new_package_from_data(
            Data::Package(MovePackage::new_system(
                SequenceNumber::new(),
                &modules,
                deps,
            )),
            TransactionDigest::genesis_marker(),
        );
        self.db.commit_single_object(movy_package)?;
        Ok(())
    }

    pub fn install_std(&self, test: bool) -> Result<(), MovyError> {
        // This is pretty hacky but works
        let stds = if test {
            movy_sui_stds::testing_std()
        } else {
            movy_sui_stds::sui_std()
        };

        let flag = if test { "testing" } else { "non-testing" };
        for out in stds {
            let out = out.movy_mock()?;
            if out.package_id != ObjectID::ZERO {
                tracing::info!("Committing {} std {}", flag, out.package_id);
                tracing::debug!(
                    "Modules are {}",
                    out.all_modules_iter()
                        .map(|v| v.self_id().name().to_string())
                        .join(",")
                );
                // let std_onchain_version = self
                //     .db
                //     .get_object(&out.package_id)
                //     .ok_or_else(|| eyre!("{} not onchain?!", out.package_id))?
                //     .version();
                let (modules, dependencies) = out.into_deployment();
                let move_package = Object::new_system_package(
                    &modules,
                    SequenceNumber::from_u64(0xff),
                    dependencies,
                    TransactionDigest::genesis_marker(),
                );
                self.db.commit_single_object(move_package)?;
            }
        }

        Ok(())
    }

    pub fn install_non_testing_std(&self) -> Result<(), MovyError> {
        self.install_std(false)
    }

    pub fn mock_testing_std(&self) -> Result<(), MovyError> {
        self.install_std(true)
    }

    pub async fn fetch_package_at_address(
        &self,
        package_id: MoveAddress,
        rpc: &GraphQlDatabase,
    ) -> Result<BTreeSet<ObjectID>, MovyError> {
        let mut out = BTreeSet::new();
        if let Some(object) = rpc.get_object(package_id.into()).await? {
            tracing::info!(
                "Fetching package {}:{} from chain",
                package_id,
                object.version()
            );
            let pkg = object
                .data
                .try_as_package()
                .ok_or_else(|| eyre!("Expected package data for {}", object.id()))?;

            for (id, upgrade_info) in pkg.linkage_table() {
                if self.db.get_object(&upgrade_info.upgraded_id).is_none() {
                    tracing::info!(
                        "Fetching ugprade cap {}:{} from chain",
                        upgrade_info.upgraded_id,
                        upgrade_info.upgraded_version
                    );
                    self.deploy_object_id(upgrade_info.upgraded_id.into(), rpc)
                        .await?;
                } else {
                    tracing::debug!("Upgrade info {:?} already exists", upgrade_info);
                }
                out.insert(*id);
            }
            self.db.commit_single_object(object)?;
        } else {
            return Err(eyre!("package {} not found", package_id).into());
        }
        Ok(out)
    }

    pub async fn load_local(
        &self,
        path: &Path,
        deployer: MoveAddress,
        attacker: MoveAddress,
        epoch: u64,
        epoch_ms: u64,
        gas: ObjectID,
        unpublished: bool,
        verify_deps: bool,
        trace_movy_init: bool,
        rpc: &GraphQlDatabase,
    ) -> Result<(MoveAddress, MovePackageAbi, MovePackageAbi, Vec<String>), MovyError> {
        tracing::info!("Compiling {} with non-test mode...", path.display());
        let abi_result = SuiCompiledPackage::build_checked(path, false, unpublished, verify_deps)?;
        let mut non_test_abi = abi_result.abi()?;
        tracing::info!("Compiled summary: {}", &abi_result);
        tracing::info!("Compiling {} with test mode...", path.display());
        let compiled_result =
            SuiCompiledPackage::build_checked(path, true, unpublished, verify_deps)?;
        tracing::info!("Compiled summary: {}", &compiled_result);

        let package_names = compiled_result.package_names.clone();
        let compiled_result = compiled_result.movy_mock()?;
        let target_package_id = compiled_result.package_id;
        let mut local_package_addresses = abi_result
            .all_modules_iter()
            .map(|m| ObjectID::from(*m.address()))
            .collect::<BTreeSet<_>>();
        let original_package_id = local_package_addresses
            .first()
            .copied()
            .ok_or_else(|| eyre!("{} has no root modules after compilation", path.display()))?;
        let upgrade_mode =
            target_package_id != ObjectID::ZERO && original_package_id != target_package_id;
        if upgrade_mode {
            tracing::info!(
                "Detected package upgrade mode for {}: original-id={}, published-at={}",
                path.display(),
                original_package_id,
                target_package_id
            );
            local_package_addresses.remove(&original_package_id);
        }
        local_package_addresses.insert(target_package_id);

        // Deploy onchain deps or deps used by immediate dependencies
        let mut packages_to_deploy = abi_result
            .dependencies()
            .iter()
            .copied()
            .chain(abi_result.all_modules_iter().flat_map(|t| {
                t.immediate_dependencies()
                    .into_iter()
                    .map(|im| (*im.address()).into())
            }))
            .collect::<BTreeSet<_>>();
        if upgrade_mode {
            packages_to_deploy.insert(original_package_id);
        }
        while let Some(dep) = packages_to_deploy.pop_last() {
            let dep = AccountAddress::from(dep);
            let dep_obj: ObjectID = dep.into();

            if dep != AccountAddress::ZERO
                && !local_package_addresses.contains(&dep_obj)
                && self.db.get_object(&dep_obj).is_none()
            {
                tracing::info!(
                    "Dependency {} not found in our db for {}, trying to fetch it from onchain",
                    dep,
                    path.display()
                );
                match self.fetch_package_at_address(dep.into(), rpc).await {
                    Ok(nexts) => {
                        packages_to_deploy.extend(nexts.into_iter());
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Fail to add the object {} due to {}, this might be fine though.",
                            dep,
                            e
                        );
                    }
                }
            }
        }

        let mut executor = SuiExecutor::new(self.db.clone())?;
        let address = if upgrade_mode {
            let (mut local_modules, local_dependencies) = compiled_result.into_deployment();
            let local_module_count_before_filter = local_modules.len();
            local_modules.retain(|module| ObjectID::from(*module.address()) == original_package_id);
            if local_modules.is_empty() {
                return Err(eyre!(
                    "no local modules remain for original-id {} after filtering upgrade payload for {}",
                    original_package_id,
                    path.display()
                )
                .into());
            }
            if local_modules.len() != local_module_count_before_filter {
                tracing::warn!(
                    "Filtered local upgrade payload modules for {}: kept {} modules at original-id {}, dropped {} non-root modules",
                    path.display(),
                    local_modules.len(),
                    original_package_id,
                    local_module_count_before_filter - local_modules.len()
                );
            }
            for module in local_modules.iter_mut() {
                mock_module_address(ObjectID::ZERO, module);
            }
            let mut local_module_bytes = Vec::with_capacity(local_modules.len());
            for module in local_modules {
                let mut buf = vec![];
                module.serialize_with_version(module.version, &mut buf)?;
                local_module_bytes.push(buf);
            }

            let mut replay_steps: Vec<OnchainUpgradeStep> = vec![];
            let mut chain_start_package_id = original_package_id;
            if rpc.get_object(target_package_id.into()).await?.is_some() {
                let mut replay_steps_rev: Vec<OnchainUpgradeStep> = vec![];
                let mut walk_to = target_package_id;
                let mut walk_done = false;
                for hop in 0..64usize {
                    if walk_to == original_package_id {
                        chain_start_package_id = original_package_id;
                        walk_done = true;
                        break;
                    }
                    let current_pkg_object = rpc
                        .get_object(walk_to.into())
                        .await?
                        .ok_or_else(|| eyre!("onchain package {} not found", walk_to))?;
                    let walk_tx_digest = current_pkg_object.previous_transaction;
                    let walk_tx = rpc
                        .transaction(&walk_tx_digest.to_string())
                        .await?
                        .ok_or_else(|| eyre!("tx {} not found", walk_tx_digest))?;
                    if let Some((step_modules, step_dependencies, package_arg)) =
                        extract_upgrade_command(&walk_tx.tx)
                    {
                        if package_arg == walk_to {
                            return Err(eyre!(
                                "invalid onchain upgrade chain at tx {}: from == to == {}",
                                walk_tx_digest,
                                walk_to
                            )
                            .into());
                        }
                        replay_steps_rev.push(OnchainUpgradeStep {
                            chain_from: package_arg,
                            chain_to: walk_to,
                            source_tx: walk_tx_digest,
                            source_checkpoint: walk_tx.checkpoint,
                            modules: step_modules,
                            dependencies: step_dependencies,
                            use_local_modules: false,
                        });
                        walk_to = package_arg;
                        if hop == 63 && walk_to != original_package_id {
                            return Err(eyre!(
                                "upgrade chain from {} to {} exceeds 64 hops",
                                original_package_id,
                                target_package_id
                            )
                            .into());
                        }
                    } else {
                        tracing::info!(
                            "Stop walking upgrade chain at tx {} for package {} (no Command::Upgrade); treat {} as chain start",
                            walk_tx_digest,
                            walk_to,
                            walk_to
                        );
                        chain_start_package_id = walk_to;
                        walk_done = true;
                        break;
                    }
                }
                if !walk_done && walk_to == original_package_id {
                    chain_start_package_id = original_package_id;
                    walk_done = true;
                }
                if !walk_done {
                    return Err(eyre!(
                        "upgrade chain walk for {} did not terminate cleanly (last package {})",
                        target_package_id,
                        walk_to
                    )
                    .into());
                }
                if chain_start_package_id != original_package_id {
                    tracing::warn!(
                        "Upgrade storage chain start {} differs from local original-id {} for {}",
                        chain_start_package_id,
                        original_package_id,
                        path.display()
                    );
                }
                replay_steps_rev.reverse();
                replay_steps = replay_steps_rev;
            } else {
                tracing::warn!(
                    "published-at {} not found on chain, fallback to a single local upgrade step",
                    target_package_id
                );
            }

            if replay_steps.is_empty() {
                replay_steps.push(OnchainUpgradeStep {
                    chain_from: original_package_id,
                    chain_to: target_package_id,
                    source_tx: TransactionDigest::genesis_marker(),
                    source_checkpoint: 0,
                    modules: local_module_bytes.clone(),
                    dependencies: local_dependencies.clone(),
                    use_local_modules: true,
                });
            } else {
                let last = replay_steps
                    .last_mut()
                    .ok_or_else(|| eyre!("empty replay steps"))?;
                last.modules = local_module_bytes.clone();
                last.dependencies = local_dependencies.clone();
                last.use_local_modules = true;
                tracing::info!(
                    "Resolved {} upgrade replay steps from {} to {} (final step uses local modules)",
                    replay_steps.len(),
                    original_package_id,
                    target_package_id
                );
            }

            if self.db.get_object(&chain_start_package_id).is_none() {
                tracing::info!(
                    "Chain start package {} not in db, fetching from chain",
                    chain_start_package_id
                );
                self.fetch_package_at_address(chain_start_package_id.into(), rpc)
                    .await?;
            }
            let chain_start_pkg = self.db.get_object(&chain_start_package_id).ok_or_else(|| {
                eyre!(
                    "Chain start package {} is not present",
                    chain_start_package_id
                )
            })?;
            let publish_tx_digest = chain_start_pkg.previous_transaction;
            tracing::info!(
                "Resolving upgrade cap from chain start package {} publish tx {}",
                chain_start_package_id,
                publish_tx_digest
            );
            let publish_tx = rpc
                .transaction(&publish_tx_digest.to_string())
                .await?
                .ok_or_else(|| eyre!("tx {} not found", publish_tx_digest))?;

            let mut resolved_cap: Option<Object> = None;
            for (obj_ref, _owner, _kind) in publish_tx.effects.all_changed_objects() {
                let Some(object) = rpc
                    .get_object_at_checkpoint(obj_ref.0, publish_tx.checkpoint)
                    .await?
                else {
                    continue;
                };
                if !object.type_().is_some_and(|ty| ty.is_upgrade_cap()) {
                    continue;
                }
                let Some(move_obj) = object.data.try_as_move() else {
                    continue;
                };
                let cap: UpgradeCap = bcs::from_bytes(move_obj.contents())?;
                if cap.package.bytes == chain_start_package_id {
                    resolved_cap = Some(object);
                    break;
                }
            }

            let mut upgrade_cap_object = resolved_cap.ok_or_else(|| {
                eyre!(
                    "No UpgradeCap found for chain start package {} in publish tx {}",
                    chain_start_package_id,
                    publish_tx_digest
                )
            })?;
            tracing::info!(
                "Selected UpgradeCap {} for chain start package {}",
                upgrade_cap_object.id(),
                chain_start_package_id
            );
            if upgrade_cap_object.get_single_owner() != Some(deployer.into()) {
                tracing::info!(
                    "Hooking UpgradeCap {} owner from {:?} to {}",
                    upgrade_cap_object.id(),
                    upgrade_cap_object.owner(),
                    deployer
                );
                upgrade_cap_object.transfer(deployer.into());
                self.db.commit_single_object(upgrade_cap_object.clone())?;
            }
            let cap_id = upgrade_cap_object.id();

            let mut effective_package_id = original_package_id;
            for (step_idx, step) in replay_steps.iter().enumerate() {
                let step_num = step_idx + 1;
                let mut current_cap_object = self.db.get_object(&cap_id).ok_or_else(|| {
                    eyre!(
                        "UpgradeCap {} missing before replay step {}/{}",
                        cap_id,
                        step_num,
                        replay_steps.len()
                    )
                })?;
                if current_cap_object.get_single_owner() != Some(deployer.into()) {
                    tracing::info!(
                        "Hooking UpgradeCap {} owner from {:?} to {} before step {}/{}",
                        current_cap_object.id(),
                        current_cap_object.owner(),
                        deployer,
                        step_num,
                        replay_steps.len()
                    );
                    current_cap_object.transfer(deployer.into());
                    self.db.commit_single_object(current_cap_object.clone())?;
                }
                let current_cap_move = current_cap_object.data.try_as_move().ok_or_else(|| {
                    eyre!(
                        "UpgradeCap object {} is not a Move object",
                        current_cap_object.id()
                    )
                })?;
                let current_cap: UpgradeCap = bcs::from_bytes(current_cap_move.contents())?;
                tracing::info!(
                    "UpgradeCap snapshot before step {}/{}: cap_id={}, cap.package={}, cap.policy={}, cap.owner={:?}",
                    step_num,
                    replay_steps.len(),
                    current_cap_object.id(),
                    current_cap.package.bytes,
                    current_cap.policy,
                    current_cap_object.owner()
                );
                if current_cap.package.bytes != step.chain_from {
                    return Err(eyre!(
                        "upgrade replay chain mismatch at step {}/{}: cap.package={} but expected chain_from={}",
                        step_num,
                        replay_steps.len(),
                        current_cap.package.bytes,
                        step.chain_from
                    )
                    .into());
                }

                if self.db.get_object(&current_cap.package.bytes).is_none() {
                    tracing::info!(
                        "Current package {} not in db, fetching from chain before step {}/{}",
                        current_cap.package.bytes,
                        step_num,
                        replay_steps.len()
                    );
                    self.fetch_package_at_address(current_cap.package.bytes.into(), rpc)
                        .await?;
                }

                let module_bytes = step.modules.clone();
                let dependencies = step.dependencies.clone();
                for dep in &dependencies {
                    if *dep != ObjectID::ZERO && self.db.get_object(dep).is_none() {
                        tracing::info!(
                            "Replay step {}/{} missing dependency {}, fetching from chain",
                            step_num,
                            replay_steps.len(),
                            dep
                        );
                        self.fetch_package_at_address((*dep).into(), rpc).await?;
                    }
                }

                let package_digest = MovePackage::compute_digest_for_modules_and_deps(
                    &module_bytes,
                    &dependencies,
                    true,
                );
                let mut upgrade_digest = package_digest.to_vec();
                // Custom runtime extension: append desired storage package ID for upgrade target.
                upgrade_digest.extend_from_slice(&step.chain_to.to_vec());

                let mut builder = ProgrammableTransactionBuilder::new();
                let cap_input = builder.obj(ObjectArg::ImmOrOwnedObject(
                    current_cap_object.compute_object_reference(),
                ))?;
                let policy_arg = builder.pure(current_cap.policy)?;
                let digest_arg = builder.pure(upgrade_digest)?;
                let upgrade_ticket = builder.programmable_move_call(
                    MoveAddress::two().into(),
                    Identifier::from_str("package").unwrap(),
                    Identifier::from_str("authorize_upgrade").unwrap(),
                    vec![],
                    vec![cap_input, policy_arg, digest_arg],
                );
                let upgrade_receipt = builder.command(Command::Upgrade(
                    module_bytes,
                    dependencies,
                    current_cap.package.bytes,
                    upgrade_ticket,
                ));
                builder.programmable_move_call(
                    MoveAddress::two().into(),
                    Identifier::from_str("package").unwrap(),
                    Identifier::from_str("commit_upgrade").unwrap(),
                    vec![],
                    vec![Argument::Input(0), upgrade_receipt],
                );

                if step.use_local_modules {
                    tracing::info!(
                        "Running final local replay step {}/{}: {} -> {}",
                        step_num,
                        replay_steps.len(),
                        step.chain_from,
                        step.chain_to
                    );
                } else {
                    tracing::info!(
                        "Running onchain replay step {}/{} (tx {}, checkpoint {}): {} -> {}",
                        step_num,
                        replay_steps.len(),
                        step.source_tx,
                        step.source_checkpoint,
                        step.chain_from,
                        step.chain_to
                    );
                }

                let out = executor.run_ptb_with_gas::<NopTracer>(
                    builder.finish(),
                    epoch,
                    epoch_ms,
                    deployer.into(),
                    gas,
                    None,
                )?;
                let effects = out.results.effects;
                let store = out.results.store;
                if !effects.status().is_ok() {
                    return Err(eyre!(
                        "fail to replay upgrade step {}/{} ({} -> {}) with {:?}",
                        step_num,
                        replay_steps.len(),
                        step.chain_from,
                        step.chain_to,
                        effects.status()
                    )
                    .into());
                }

                let mut upgraded_package = None;
                for t in effects.all_changed_objects() {
                    if matches!(&t.2, WriteKind::Create)
                        && let Some(object) = store.written.get(&t.0.0)
                        && object.is_package()
                    {
                        upgraded_package = Some(t.0.0);
                    }
                }
                let upgraded_package_id = upgraded_package.ok_or_else(|| {
                    eyre!(
                        "replay step {}/{} succeeds but no package object created",
                        step_num,
                        replay_steps.len()
                    )
                })?;
                if upgraded_package_id != step.chain_to {
                    return Err(eyre!(
                        "replay step {}/{} storage id mismatch: expected {}, got {}",
                        step_num,
                        replay_steps.len(),
                        step.chain_to,
                        upgraded_package_id
                    )
                    .into());
                }
                self.db.commit_store(store, &effects)?;
                effective_package_id = upgraded_package_id;
            }

            if effective_package_id != target_package_id {
                return Err(eyre!(
                    "upgrade replay ends at {}, expected published-at {}",
                    effective_package_id,
                    target_package_id
                )
                .into());
            }
            effective_package_id
        } else {
            executor.deploy_contract(epoch, epoch_ms, deployer.into(), gas, compiled_result)?
        };

        // In search of any deploy functions
        let mut abi = self.db.get_package_info(address.into())?.unwrap();

        for md in abi.modules.iter() {
            if md.is_test_only_module()
                && let Some(init) = md.locate_movy_init()
            {
                let mut builder = ProgrammableTransactionBuilder::new();
                let deployer_arg = builder.pure(ObjectID::from(deployer))?;
                let attacker_arg = builder.pure(ObjectID::from(attacker))?;
                builder.programmable_move_call(
                    address,
                    Identifier::from_str(&md.module_id.module_name).unwrap(),
                    Identifier::from_str(&init.name).unwrap(),
                    vec![],
                    vec![deployer_arg, attacker_arg],
                );
                let ptb = builder.finish();
                tracing::info!("Detected a {} at: {}", MOVY_INIT, md.module_id);
                let tracer = if trace_movy_init {
                    Some(TreeTracer::new())
                } else {
                    None
                };
                let mut results = executor.run_ptb_with_movy_tracer_gas(
                    ptb,
                    epoch,
                    epoch_ms,
                    deployer.into(),
                    gas,
                    tracer,
                )?;
                let trace =
                    std::mem::take(&mut results.tracer).map(|tracer| tracer.take_inner().pprint());
                if !results.effects.status().is_ok() {
                    if let Some(trace) = trace {
                        tracing::error!("movy_init reverts with:\n{}", trace);
                    }
                    return Err(eyre!("movy_init reverts!").into());
                }
                tracing::trace!(
                    "movy_init trace:\n{}",
                    trace.unwrap_or_else(|| "-".to_string())
                );
                tracing::info!("Commiting movy_init effects...");
                tracing::debug!(
                    "Status: {:?} Changed Objects: {}, Removed Objects: {}",
                    results.effects.status(),
                    results
                        .effects
                        .all_changed_objects()
                        .iter()
                        .map(|t| format!("{:?}", t))
                        .join(","),
                    results
                        .effects
                        .all_removed_objects()
                        .iter()
                        .map(|t| format!("{:?}", t.0))
                        .join(",")
                );
                self.db
                    .commit_store(results.results.store, &results.results.effects)?;
            }
        }

        non_test_abi.published_at(address.into());
        abi.published_at(address.into());
        Ok((address.into(), abi, non_test_abi, package_names))
    }

    pub async fn export_abi(&self) -> Result<BTreeMap<MoveAddress, MovePackageAbi>, MovyError> {
        let objects = self.db.list_objects().await?;

        let mut out = BTreeMap::new();
        for obj in objects {
            if let Ok(Some(abi)) = self.db.get_package_info(obj) {
                // object is package
                out.insert(abi.package_id, abi);
            }
        }
        Ok(out)
    }

    pub async fn load_history(
        &self,
        package_id: MoveAddress,
        ckpt: u64,
        rpc: &GraphQlClient,
    ) -> Result<(), MovyError> {
        if let Some(package) = self.db.get_package_info(package_id)? {
            for module in &package.modules {
                for s in &module.structs {
                    let tag = s.module_id.to_canonical_string(true);
                    let objects = rpc
                        .filter_objects(ckpt, Some(OwnerKind::Shared), None, Some(tag))
                        .await?;
                    for object in objects.into_iter() {
                        self.db.commit_single_object(object)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn load_inner_types(&self) -> Result<(), MovyError> {
        // Analyze all object types in the store
        let objects = self.db.list_objects().await?;
        for obj in objects {
            if let Ok(mv) = self.db.get_move_object_info(obj) {
                let addresses = mv.ty.flat_addresses();
                for addr in addresses {
                    self.db.load_object(addr).await?
                }
            }
        }
        Ok(())
    }

    pub async fn deploy_object_id(
        &self,
        package_id: MoveAddress,
        rpc: &GraphQlDatabase,
    ) -> Result<(), MovyError> {
        if let Some(object) = rpc.get_object(package_id.into()).await? {
            self.db.commit_single_object(object)?;
        } else {
            return Err(eyre!("object {} not found", package_id).into());
        }

        Ok(())
    }

    pub async fn all_tys(&self) -> Result<BTreeSet<MoveStructTag>, MovyError> {
        let mut tags = BTreeSet::new();
        for obj in self.db.list_objects().await? {
            if let Ok(info) = self.db.get_move_object_info(obj) {
                for st in info.ty.flat_structs() {
                    tags.insert(st);
                }
            }
        }
        Ok(tags)
    }
}
