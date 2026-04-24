# Movy

![movy](./movy.jpg)

**Movy** is a Move testing framework that offers:

- Modular low-level building bricks for Move language. Specifically, the executor and tracer abstractions and layered database design borrowed from [revm](https://github.com/bluealloy/revm) that allow you to emulate and inspect an execution.
- Static analysis capabilities inherited from [MoveScan](https://dl.acm.org/doi/10.1145/3650212.3680391), the state-of-the-art static analyzer.
- Cutting-edge fuzzing reimplemented from scratch learned from [Belobog](https://github.com/abortfuzz/belobog) that supports both property testing and on-chain fuzzing, in the a flavor similar to [foundry](https://getfoundry.sh/forge/advanced-testing/overview) by writing invariants in Move language.
- And a lot of more...

Checkout our documentations at [here](https://docs.movy.rs)

__Movy is still in very early-alpha stage and we are working heavily for new features__

## Show cases

### Trace a Transaction

```rust
let mut tracer = TreeTracer::new();
let _ = executor.run_tx_trace(
    tx,
    epoch,
    timestamp_ms,
    Some(tracer),
)?;
println!("The trace is:\n{}", trace.take_inner().pprint());
```

This snippet traces an arbitrary transaction `tx`, either on-chain or built by yourself.

### Invariants Testing

Deploy your Move modules in a single function, even if it requires multiple transactions.

```move
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
        assert!(counter::value(&counter_val) == 1, 0);
        ts::return_shared(counter_val);
    };

    ts::end(scenario);
}
```

Write an invariant test for your functions in a Move testing module:

```move
#[test]
public fun movy_pre_increment(
    movy: &mut context::MovyContext,
    ctr: &mut Counter,
    _n: u64
) {
    let (ctr_id, val) = extract_counter(ctr);
    let state = context::borrow_mut_state(movy);
    bag::add(state, ctr_id, val);
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
    if (*previous_val + n != new_val) {
        crash_because(b"Increment does not correctly increases internal value.".to_string());
    }
}
```

### Call Graph and Type Graph 

Generate a type graph for a move package.

![type graph](./tg.svg)

Generate a call graph for a move package.

![call graph](./cg.svg)

### Static Analysis

TODO.

## Usage

### Use Movy as a Tool

Install dependencies:

```bash
apt install -y libssl-dev libclang-dev
```

Build `movy` binaries.

```bash
git clone https://github.com/BitsLabSec/movy
cd movy
cargo build --release
```

Note a stable rust toolchain should be present.

Check the usage menu.

```bash
./target/release/movy --help
```

### Use Movy as a Library

Add this to your `Cargo.toml`

```toml
movy = {git = "https://github.com/BitsLabSec/movy", branch = "master"}
```

Unfortunately, both `sui` and `aptos` are not on `crates.io` so we can not publish crates at this moment, unless we fully re-implement the MoveVM for both chains.

### Write Invariants

To write invariants for contracts, see [the counter sample](./test-data/counter/tests/movy.move). Note you need to add the line to your `Move.toml`. It is test dependency and will be never live on-chain.

```toml
[dev-dependencies]
movy = {git = "https://github.com/BitsLabSec/movy", subdir = "move/movy", rev = "master"}
```

## Contributions

**Movy** is very open to contributions! We expect your feedbacks and pull requests. See the roadmap or contact us for further information.

## Roadmap

At this moment, `movy` is in very early-alpha state with the folloing features missing:

- Upstream our changes to [sui](https://github.com/MystenLabs/sui) and [aptos-core](https://github.com/aptos-labs/aptos-core)
- Full Aptos support. (We have a private branch for that but still figuring out a good API design.)
- On-chain incidents backtesting.

## Credits

Movy is inspired by several pioneering projects:

- [Belobog](https://github.com/abortfuzz/belobog)
- [ityfuzz](https://github.com/fuzzland/ityfuzz)
- [move-fuzzer](https://github.com/fuzzland/move-fuzzer)
- [sui-fuzzer](https://github.com/FuzzingLabs/sui-fuzzer)
- [historical-dev-inspect](https://github.com/kklas/historical-dev-inspect)