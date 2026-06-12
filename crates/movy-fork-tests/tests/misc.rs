//! Guards for the miscellaneous fork patches.
//!
//! Covered commits (see FORK_TESTS.md):
//!   #2  ecc7f94cb9 — `pub mod transfer` in the **v2** sui-move-natives cut
//!   #12 21a1c5f944 — `ExecutionMode::targeted_deployment()` hook (publish at a chosen ObjectID)
//!
//! Not guarded here (documented as non-semantic in FORK_TESTS.md):
//!   #13 0cf08add03 — protocol-config override `warn!` → `debug!` (log level only)
//!   the `tracing::warn!` hunks added inside #12 (diagnostics only)

// #2: the v2 cut exposes its `transfer` natives module. This `use` only compiles while the module
// is `pub`; if the patch is dropped (`mod transfer;`) it fails with E0603 (module is private).
// movy's own (currently commented-out) raw-transfer cheat targets this surface — see
// `crates/movy-sui/src/cheats/scenario.rs`. The import is intentionally unused.
#[allow(unused_imports)]
use sui_move_natives_v2::transfer as _v2_transfer_is_public;

use movy_fork_tests::harness;
use sui_types::base_types::ObjectID;

/// #12: publishing a package with a *pinned* target id deploys it at exactly that id instead of a
/// freshly-derived one. This drives `SuiFuzzMode::targeted_deployment` (movy-replay/src/exec.rs),
/// which reads the per-digest target installed by `deploy_contract`'s `target` path. Without the
/// `ExecutionMode::targeted_deployment` hook the package lands at a random id and `deploy_contract`
/// returns an error (its internal `deployed at != target` check), failing this test.
#[test]
fn targeted_deployment_pins_package_id() {
    let mut h = harness();
    let pkg = h
        .compile(
            "tgt",
            "tgt",
            r#"module tgt::tgt;
public fun answer(): u64 { 42 }
"#,
        )
        .expect("compile targeted package");

    let target = ObjectID::from_hex_literal(
        "0x00000000000000000000000000000000000000000000000000000000cafef00d",
    )
    .expect("valid target id");

    let (pkg_id, _cap) = h
        .deploy_at(pkg, target)
        .expect("deploy at pinned target should succeed (patch #12)");

    assert_eq!(
        pkg_id, target,
        "targeted deployment must place the package at the pinned id"
    );
    assert!(
        h.db_has_package(target),
        "pinned package {target} should be committed in the store"
    );
}
