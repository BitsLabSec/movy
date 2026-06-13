//! Shared, fully-offline, in-process test harness for the movy ⇄ sui fork-patch guards.
//!
//! Mirrors the offline path of `movy sui build-deploy` (see `crates/movy/src/sui/deploy.rs`):
//! a writable in-memory [`CachedStore`] backed by an [`EmptyStore`] (no network), shared via an
//! `Arc` between a [`SuiTestingEnv`] (used once to install the testing std + the `movy` package)
//! and a [`SuiExecutor`] (used to compile, deploy and run programmable transactions).
//!
//! Every `tests/*.rs` file uses this harness to exercise movy's *real* consumption of a fork
//! patch and assert the semantic it provides. See `FORK_TESTS.md` for the commit ↔ guard map.

use std::sync::Arc;

use std::io::Write as _;

use anyhow::{Context, Result};
use movy_replay::{
    db::ObjectStoreMintObject,
    env::SuiTestingEnv,
    exec::{SuiExecutor, very_big_gas},
    tracer::{MovySuiTracerExt, NopTracer},
};
use movy_sui::{
    compile::{BuildIsolation, SuiCompiledPackage},
    database::{cache::CachedStore, empty::EmptyStore},
};
use movy_types::{
    input::{MoveAddress, MoveTypeTag},
    object::MoveOwner,
};
use serde::Serialize;
use sui_types::{
    Identifier, TypeTag,
    base_types::{ObjectID, SuiAddress},
    effects::TransactionEffectsAPI,
    programmable_transaction_builder::ProgrammableTransactionBuilder,
    transaction::ProgrammableTransaction,
};

/// The concrete fully-offline store type shared by the env and the executor.
pub type ForkDb = Arc<CachedStore<EmptyStore>>;

pub struct Harness {
    pub db: ForkDb,
    pub executor: SuiExecutor<ForkDb>,
    pub deployer: MoveAddress,
    pub gas: ObjectID,
    pub epoch: u64,
    pub epoch_ms: u64,
}

/// Build a fresh offline harness: empty in-memory store + minted gas + testing std + `movy` pkg.
pub fn harness() -> Harness {
    // Keep logs quiet but allow `RUST_LOG=...` to turn them on for debugging a guard.
    let _ = tracing_subscriber_try_init();

    let store = CachedStore::new(EmptyStore);
    let deployer = MoveAddress::from_str("0xA11CE").expect("valid address");

    // Mint a single large gas coin owned by the deployer.
    let sui_tag: MoveTypeTag = "0x2::sui::SUI".parse().expect("sui type tag");
    let gas = store
        .mint_coin(
            sui_tag,
            MoveOwner::AddressOwner(deployer),
            very_big_gas(),
        )
        .expect("mint gas");

    // Share the store between the env (install std/movy) and the executor.
    let db = store.wrapped();
    let tenv = SuiTestingEnv::new(db.clone());
    tenv.mock_testing_std().expect("install testing std");
    tenv.install_movy().expect("install movy package");

    let executor = SuiExecutor::new(db.clone()).expect("build executor");

    Harness {
        db,
        executor,
        deployer,
        gas: gas.into(),
        epoch: 1,
        epoch_ms: 1,
    }
}

impl Harness {
    pub fn deployer_sui(&self) -> SuiAddress {
        self.deployer.into()
    }

    /// Whether `id` is a Move package committed in the store.
    pub fn db_has_package(&self, id: ObjectID) -> bool {
        use sui_types::storage::ObjectStore;
        self.db
            .get_object(&id)
            .map(|o| o.is_package())
            .unwrap_or(false)
    }

    /// Compile inline Move source into a package (unpublished, address `0x0`), **non-test** mode.
    pub fn compile(&self, package: &str, module: &str, source: &str) -> Result<SuiCompiledPackage> {
        SuiCompiledPackage::build_quick(package, module, source)
            .with_context(|| format!("compiling inline package {package}::{module}"))
    }

    /// Compile inline Move source in **test mode** so `#[test_only]` framework modules such as
    /// `sui::test_scenario` are available. Mirrors `SuiCompiledPackage::build_quick` but passes
    /// `test_mode = true` (see `crates/movy-sui/src/compile.rs`).
    pub fn compile_test(
        &self,
        package: &str,
        module: &str,
        source: &str,
    ) -> Result<SuiCompiledPackage> {
        let dir = tempfile::TempDir::new()?;
        let toml = format!(
            "[package]\nname = \"{package}\"\nedition = \"2024.beta\"\n\n[dependencies]\n\n[addresses]\n{package} = \"0x0\"\n\n[dev-dependencies]\n\n[dev-addresses]\n"
        );
        let mut fp = std::fs::File::create(dir.path().join("Move.toml"))?;
        fp.write_all(toml.as_bytes())?;
        std::fs::create_dir_all(dir.path().join("sources"))?;
        let mut fp = std::fs::File::create(dir.path().join(format!("sources/{module}.move")))?;
        fp.write_all(source.as_bytes())?;
        let pkg = SuiCompiledPackage::build(
            dir.path(),
            /* test_mode */ true,
            /* with_unpublished */ true,
            &BuildIsolation::default(),
        )
        .with_context(|| {
            format!("compiling (test mode) inline package {package}::{module}")
                })?;
        // Test-mode modules are serialized as "unpublishable"; the publish-time deserializer
        // rejects them unless flipped to publishable. This mirrors what movy does for every
        // test-mode package it deploys (`SuiCompiledPackage::movy_mock`, compile.rs).
        pkg.movy_mock()
            .with_context(|| format!("making {package}::{module} publishable"))
    }

    /// Build a programmable transaction that calls `pkg::module::func` with homogeneous pure args.
    pub fn call_pure_args<S: Serialize>(
        &self,
        pkg: ObjectID,
        module: &str,
        func: &str,
        ty_args: Vec<TypeTag>,
        args: &[S],
    ) -> Result<ProgrammableTransaction> {
        let mut builder = ProgrammableTransactionBuilder::new();
        let mut call_args = Vec::with_capacity(args.len());
        for arg in args {
            call_args.push(builder.pure(arg)?);
        }
        builder.programmable_move_call(
            pkg,
            Identifier::new(module)?,
            Identifier::new(func)?,
            ty_args,
            call_args,
        );
        Ok(builder.finish())
    }

    /// Run a programmable transaction through the testing path without a tracer; returns success.
    pub fn run_testing_no_tracer(&self, ptb: ProgrammableTransaction) -> Result<bool> {
        let (_t, ok) = self.run_testing::<NopTracer>(ptb, None)?;
        Ok(ok)
    }

    /// Deploy a compiled package at a freshly-derived id. Returns `(package_id, upgrade_cap)`.
    pub fn deploy(&mut self, pkg: SuiCompiledPackage) -> Result<(ObjectID, ObjectID)> {
        let (epoch, epoch_ms, deployer, gas) =
            (self.epoch, self.epoch_ms, self.deployer_sui(), self.gas);
        self.executor
            .deploy_contract(epoch, epoch_ms, deployer, gas, pkg)
            .map_err(Into::into)
    }

    /// Deploy a compiled package *pinned* to `target` (exercises targeted deployment, patch #12).
    /// Returns `(package_id, upgrade_cap)`; the caller asserts `package_id == target`.
    pub fn deploy_at(
        &mut self,
        mut pkg: SuiCompiledPackage,
        target: ObjectID,
    ) -> Result<(ObjectID, ObjectID)> {
        pkg.package_id = target;
        self.deploy(pkg)
    }

    /// Run a programmable transaction through the *testing* path (testing feature on, patch #4),
    /// optionally with a movy tracer. Returns the tracer (if any) and whether execution succeeded.
    pub fn run_testing<R: MovySuiTracerExt>(
        &self,
        ptb: ProgrammableTransaction,
        tracer: Option<R>,
    ) -> Result<(Option<R>, bool)> {
        let (epoch, epoch_ms, sender, gas) =
            (self.epoch, self.epoch_ms, self.deployer_sui(), self.gas);
        let out = self
            .executor
            .run_ptb_with_movy_testing_tracer_gas(ptb, epoch, epoch_ms, sender, gas, tracer)?;
        let ok = out.results.effects.status().is_ok();
        Ok((out.tracer, ok))
    }
}

fn tracing_subscriber_try_init() -> Result<()> {
    // Quiet by default; set e.g. `RUST_LOG=warn,movy_replay=debug` + `--nocapture` to see why a
    // deploy/publish fails (the fork's #12 patch logs the missing module at warn level).
    use tracing_subscriber::EnvFilter;
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error")))
        .with_test_writer()
        .try_init();
    Ok(())
}

// Re-export the conversion helper used by tests.
pub use movy_types::input::MoveAddress as ForkAddress;
