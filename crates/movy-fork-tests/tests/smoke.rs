//! Foundation smoke test: the offline harness boots and can compile + deploy a trivial package.
//! If this fails, every other guard in this crate is moot — fix the harness first.

use movy_fork_tests::harness;

#[test]
fn harness_boots_and_deploys() {
    let mut h = harness();

    let pkg = h
        .compile(
            "smoke",
            "smoke",
            r#"module smoke::smoke;
public struct Thing has key, store { id: object::UID }
public fun make(ctx: &mut TxContext) {
    transfer::public_transfer(Thing { id: object::new(ctx) }, ctx.sender());
}
"#,
        )
        .expect("compile trivial package");

    let (pkg_id, cap) = h.deploy(pkg).expect("deploy trivial package");
    assert_ne!(pkg_id, cap, "package id and upgrade cap must differ");
    assert!(
        h.db_has_package(pkg_id),
        "deployed package {pkg_id} should be committed to the store"
    );
}
