#[test_only]
module counter::counter_tests;

use sui::test_scenario::{Self as ts};
use counter::counter::{Self, Counter};
use movy::context::Self;
use movy::oracle::crash_because;
use sui::bag::Self;
use movy::log::log_keyed_u64;

#[test]
fun test_counter_smoke() {
    assert!(1 + 1 == 2, 0);
}

#[test]
public fun movy_init(
    deployer: address,
    attacker: address
) {
    let mut scenario = ts::begin(deployer);
    {
        ts::next_tx(&mut scenario, deployer);
        counter::create(ts::ctx(&mut scenario));
    };

    ts::next_tx(&mut scenario, attacker);
    {
        let mut counter_val = ts::take_shared<Counter>(&scenario);
        counter::increment(&mut counter_val, 0);
        ts::return_shared(counter_val);
    };

    ts::end(scenario);
}


// Helper
#[test]
fun extract_counter(ctr: &Counter): (ID, u64) {
    let val = counter::value(ctr);
    let ctr_id = sui::object::id(ctr);
    (ctr_id, val)
}

// ===== Oracles =====
// PTB-wise pre- and post- conditions
#[test]
public fun movy_pre_ptb(
    movy: &mut context::MovyContext,
    ctr: &mut Counter,
) {
    let (ctr_id, val) = extract_counter(ctr);
    let state = context::borrow_mut_state(movy);
    bag::add(state, ctr_id, val);
    log_keyed_u64(b"pre-ptb".to_string(), val);
}

#[test]
public fun movy_post_ptb(
    movy: &mut context::MovyContext,
    ctr: &mut Counter,
) {
    let (ctr_id, new_val) = extract_counter(ctr);
    let state = context::borrow_state(movy);
    let previous_val = bag::borrow<ID, u64>(state, ctr_id);
    log_keyed_u64(b"post-ptb".to_string(), new_val);
    if (*previous_val > new_val) {
        crash_because(b"Counter should be always increasing".to_string());
    }
}

// Pre- and Post- conditions of a single movecall
#[test]
public fun movy_pre_increment(
    movy: &mut context::MovyContext,
    ctr: &mut Counter,
    _n: u64
) {
    let (ctr_id, val) = extract_counter(ctr);
    let state = context::borrow_mut_state(movy);
    bag::add(state, ctr_id, val);
    log_keyed_u64(b"post-increment".to_string(), val);
}

#[test]
public fun movy_post_increment(
    movy: &mut context::MovyContext,
    ctr: &mut Counter,
    n: u64
) {
    let (ctr_id, new_val) = extract_counter(ctr);
    let state = context::borrow_state(movy);
    let previous_val = bag::borrow<ID, u64>(state, ctr_id);
    log_keyed_u64(b"post-increment".to_string(), new_val);
    if (*previous_val + n != new_val) {
        crash_because(b"Increment does not correctly inreases internal value.".to_string());
    }
}
