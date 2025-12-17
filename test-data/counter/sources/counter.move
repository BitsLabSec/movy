module counter::counter;

public struct Counter has key {
    id: UID,
    value: u64
}

public fun create(ctx: &mut TxContext) {
    let counter = Counter {
        id: object::new(ctx),
        value: 1
    };
    transfer::share_object(counter);
}

public fun increment(counter: &mut Counter, n: u64) {
    counter.value = counter.value + n;
}

public fun value(counter: &Counter): u64 {
    counter.value
}