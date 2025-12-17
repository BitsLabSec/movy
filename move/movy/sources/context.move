module movy::context;

use sui::bag::{Self, Bag};
use movy::log;

public struct MovyContext has key {
    id: UID,
    state: Bag
}

fun init(ctx: &mut sui::tx_context::TxContext) {
    let state = create_context(ctx);
    share_context(state)
}

public fun create_context(ctx: &mut sui::tx_context::TxContext): MovyContext {
    MovyContext {
        id: object::new(ctx),
        state: bag::new(ctx)
    }
}

public fun share_context(state: MovyContext) {
    log::log_keyed_uid(b"movy_state".to_string(), &state.id);
    transfer::share_object(state);
}

public fun destroy_context(
    state: MovyContext,
    ctx: &mut sui::tx_context::TxContext
) {
    // not really... but works
    transfer::transfer(state, sui::tx_context::sender(ctx));
}

public fun borrow_mut_state(movy: &mut MovyContext): &mut Bag {
    &mut movy.state
}

public fun borrow_state(movy: &MovyContext): &Bag {
    &movy.state
}