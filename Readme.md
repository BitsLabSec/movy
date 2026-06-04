# Movy

![movy](./movy.jpg)

**Movy** is a Move testing framework that offers:

- Modular low-level building bricks for Move language. Specifically, the executor and tracer abstractions and layered database design borrowed from [revm](https://github.com/bluealloy/revm) that allow you to emulate and inspect an execution.
- Static analysis capabilities inherited from [MoveScan](https://dl.acm.org/doi/10.1145/3650212.3680391), the state-of-the-art static analyzer.
- Cutting-edge fuzzing reimplemented from scratch learned from [Belobog](https://github.com/abortfuzz/belobog) that supports both property testing and on-chain fuzzing, in the a flavor similar to [foundry](https://getfoundry.sh/forge/advanced-testing/overview) by writing invariants in Move language.
- A `forge test`-like runner (`movy sui test`) that executes your `test_*` functions and automatically fills their object and type-parameter arguments.
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
        crash_because(b"Increment does not correctly inreases internal value.".to_string());
    }
}
```

### Running Tests with `sui test`

`movy sui test` builds and deploys your package, runs `movy_init`, then executes every `#[test]`
function whose name starts with `test_` — much like `forge test`. Unlike the stock Move test
runner, these test functions may take **parameters**, including Sui objects and type parameters,
and `movy` fills them in for you.

```move
// test-data/counter/tests/movy.move
#[test]
fun test_counter_smoke() {
    assert!(1 + 1 == 2, 0);
}

// Object and type-parameter arguments are filled by movy:
#[test]
public fun test_increment_typed<T>(ctr: &mut Counter) {
    let _ty = std::type_name::get<T>();
    let before = counter::value(ctr);
    counter::increment(ctr, 3);
    assert!(counter::value(ctr) == before + 3, 300);
}
```

Run all tests in a package:

```bash
movy sui test --locals ./test-data/counter
```

#### Discover objects and pending arguments: `--only-init`

`--only-init` runs `movy_init` and then prints the objects it produced together with every test
function and the arguments it still needs — the starting point for filling them in:

```bash
movy sui test --locals ./test-data/counter --only-init
```

```
=== objects after movy_init (3) ===
deployer: 0xb641...
attacker: 0xa773...
0x95e1…  0x2::coin::Coin<0x2::sui::SUI>  [owned by 0xa773… (attacker)]  v3
0xd726…  <pkg>::counter::Counter         [shared (v3)]                  v3
0xdbcf…  0x2::package::UpgradeCap        [owned by 0xb641… (deployer)]  v2

=== test functions ===
<pkg>::counter_tests::test_counter_smoke()                                   [no args]
<pkg>::counter_tests::test_increment_typed<T0>(&mut <pkg>::counter::Counter) [needs args]
```

#### Fill arguments: `--object-mapping` and `--test-ty`

Bind an object parameter to a specific object, and pin a type parameter to a concrete type (use
the object id printed by `--only-init`):

```bash
movy sui test --locals ./test-data/counter \
  --object-mapping 'counter::counter::Counter/0xd726…e5d3' \
  --test-ty 'counter::counter_tests::test_increment_typed:0/0x2::sui::SUI'
```

- `--object-mapping <type>/0x<object_id>` — fill an object parameter with the given object.
  Repeatable (or comma-separated); entries of the same type are consumed in parameter order.
- `--test-ty <pkg::module::func>:<index>/<type>` — set type parameter `<index>` of a test function.

Types and function selectors accept **local package names** (e.g. `counter::counter::Counter`),
resolved to the currently deployed address. A mapping therefore keeps working across rebuilds even
though the deployed package id changes, and you can paste types straight out of `--only-init` or a
`--trace`. Unmapped object/type arguments fall back to automatic, fuzzer-style filling.

#### Pin deployment addresses: `--deploy-at`

Object ids produced by `movy_init` are stable across source edits, but a freshly deployed package
is assigned a new id whenever its bytecode changes — which also changes every type string
(`<pkgid>::counter::Counter`). Pin the package to a fixed address to keep ids and type strings
stable:

```bash
movy sui test --locals ./test-data/counter --deploy-at counter:0xcafe…
```

`--deploy-at <pkg_name>:0x<address>` is repeatable and matches packages by name.

#### Failures and reproducibility

A test fails (non-zero exit) when the transaction aborts, e.g. a failing `assert!`. In addition,
your `movy_pre_*` / `movy_post_*` invariants are applied while each test runs, so an invariant
violation reported with `crash_because` also fails the test and surfaces the reason:

```
oracle crash detected for <pkg>::counter_tests::test_increment_typed: Counter should be always increasing
```

`--seed` pins the RNG (and thus the gas object and freshly-derived package/object ids), while
`--checkpoint` / `--epoch` / `--epoch-ms` pin the on-chain context and let the run work fully
offline (otherwise they are fetched from `--rpc`). Pass `--trace` to print the execution trace of
each test.

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

## Contritubions

**Movy** is very open to contributions! We expect your feedbacks and pull requests. See the roadmap or contact us for further information.

## Roadmap

At this moment, `movy` is in very early-alpha state with the folloing features missing:

- Upstream our changes to [sui](https://github.com/MystenLabs/sui) and [aptos-core](https://github.com/aptos-labs/aptos-core)
- Full Aptos support. (We have a private branch for that but still figuring out a good API design.)
- On-chain incidents backtesting.

## Credits

Belobog is inspired by several pioneering projects:

- [Belobog](https://github.com/abortfuzz/belobog)
- [ityfuzz](https://github.com/fuzzland/ityfuzz)
- [move-fuzzer](https://github.com/fuzzland/move-fuzzer)
- [sui-fuzzer](https://github.com/FuzzingLabs/sui-fuzzer)
- [historical-dev-inspect](https://github.com/kklas/historical-dev-inspect)