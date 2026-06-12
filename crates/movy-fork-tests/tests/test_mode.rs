//! Guards for the "force test-mode support" fork patches.
//!
//! Covered commits (see FORK_TESTS.md):
//!   #4  bcfc8c2f80 — testing feature: `sui::test_scenario` natives usable in real PT execution
//!   #5  dc264e518c — allow `init` to be called (verify_init_not_called disabled)
//!   #6  6d728ea5ae — allow manual one-time-witness instantiation (verify_no_instantiations disabled)
//!   #7  dbebceb0cb — ObjectRuntimeState::deep_copy field sync (exercised transitively by #4)
//!   #11 44c5372133 — end_transaction extends (not overwrites) input_objects across `next_tx`

use movy_fork_tests::harness;
use sui_types::base_types::ObjectID;

/// #5: a `#[test_only]` function calls the module's own `init`. The Move *source* compiler only
/// permits an explicit `init` call from test code, so this must be built in test mode; movy
/// publishes such test-mode packages. Upstream's `entry_points_verifier` then rejects the init call
/// at publish (`verify_init_not_called` — which, lacking FnInfo, fires even for test functions);
/// the fork comments that check out, so the publish (deploy) succeeds.
#[test]
fn init_can_be_called() {
    let mut h = harness();
    let pkg = h
        .compile_test(
            "initcall",
            "initcall",
            r#"module initcall::initcall;
public struct Thing has key, store { id: object::UID }
fun init(ctx: &mut TxContext) {
    transfer::public_transfer(Thing { id: object::new(ctx) }, ctx.sender());
}
#[test_only]
public fun call_init(ctx: &mut TxContext) {
    init(ctx);
}
"#,
        )
        .expect("compile init-call package");

    let deployed = h.deploy(pkg);
    assert!(
        deployed.is_ok(),
        "deploying a (test-mode) module that calls init must succeed with patch #5 (got {deployed:?})"
    );
}

/// #6: a `#[test_only]` function manually constructs the module's one-time witness (`MANOTW`, a
/// single-`bool` `drop` struct named after the module). Only test code may build an OTW by hand, so
/// this is a test-mode package. Upstream's `one_time_witness_verifier` rejects the instantiation at
/// publish (`verify_no_instantiations`); the fork disables it, so deploy succeeds.
#[test]
fn manual_otw_can_be_instantiated() {
    let mut h = harness();
    let pkg = h
        .compile_test(
            "manotw",
            "manotw",
            r#"module manotw::manotw;
public struct MANOTW has drop { dummy: bool }
#[test_only]
public fun forge(): MANOTW {
    MANOTW { dummy: false }
}
"#,
        )
        .expect("compile manual-OTW package");

    let deployed = h.deploy(pkg);
    assert!(
        deployed.is_ok(),
        "deploying a (test-mode) module that hand-builds its OTW must succeed with patch #6 (got {deployed:?})"
    );
}

/// #4 + #11 (+ #7): a multi-transaction `test_scenario` executed as a *real* programmable
/// transaction (not `sui move test`). It only runs at all because the testing feature is compiled
/// in (#4), and the cross-`next_tx` asserts only hold because `end_transaction` accumulates input
/// objects across transaction boundaries (#11). The `deep_copy` of runtime state (#7) is exercised
/// on every `end_transaction`.
#[test]
fn multi_tx_test_scenario_runs_in_execution() {
    let mut h = harness();
    let pkg = h
        .compile_test(
            "scen",
            "scen",
            r#"module scen::scen;
use sui::test_scenario as ts;

public struct Box has key, store { id: object::UID, v: u64 }

#[test]
public fun run(owner: address) {
    let mut s = ts::begin(owner);

    // tx1: create and share a Box.
    ts::next_tx(&mut s, owner);
    {
        let b = Box { id: object::new(ts::ctx(&mut s)), v: 7 };
        transfer::share_object(b);
    };

    // tx2: take it back across the next_tx boundary (needs #11), mutate it.
    ts::next_tx(&mut s, owner);
    {
        let mut b = ts::take_shared<Box>(&s);
        assert!(b.v == 7, 100);
        b.v = 8;
        ts::return_shared(b);
    };

    // tx3: confirm the mutation survived another boundary.
    ts::next_tx(&mut s, owner);
    {
        let b = ts::take_shared<Box>(&s);
        assert!(b.v == 8, 101);
        ts::return_shared(b);
    };

    ts::end(s);
}
"#,
        )
        .expect("compile (test mode) scenario package");

    let (pkg_id, _cap) = h.deploy(pkg).expect("deploy scenario package");

    let owner = ObjectID::from(h.deployer);
    let ptb = h
        .call_pure_args(pkg_id, "scen", "run", vec![], &[owner])
        .expect("build scenario call ptb");

    let ok = h
        .run_testing_no_tracer(ptb)
        .expect("scenario execution should not error out");
    assert!(
        ok,
        "multi-tx test_scenario must succeed in execution (patches #4 + #11 + #7)"
    );
}
