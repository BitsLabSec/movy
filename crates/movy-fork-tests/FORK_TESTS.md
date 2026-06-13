# Fork patch guards — `./sui` upgrade playbook

`movy` depends on a **fork of Sui** (`github.com/wtdcode/sui`, branch `v1.65.2-fuzz`, checked out at
`../sui`). On top of the upstream release base (`072a211161`) the fork carries **14 commits** (the
last, `7b0edf70af`, adds the two build-isolation semantics movy's per-spec audit harness needs). When
we rebase the fork onto a newer upstream Sui release we must re-apply those patches — and be sure each
one still means the same thing. This crate is the gate: every behaviour-bearing patch has a test that
**fails if the patch is dropped or its semantic regresses**.

This document maps every fork commit to the file(s) it touches, the exact semantic, and the guarding
test — so re-porting is mechanical and verifiable.

---

## Running the gate

```bash
cd movy
cargo test -p movy-fork-tests          # all guards; this is the upgrade gate
```

Tests are fully **offline and in-process** (no RPC, no validator): an in-memory `CachedStore` over an
`EmptyStore`, the testing std + `movy` package installed, a minted gas coin, and a `SuiExecutor`
(`src/lib.rs::harness`). They compile tiny Move fixtures on the fly (`build_quick` / `compile_test`).

## CI

`.github/workflows/test.yaml` runs `cargo test -p movy-fork-tests` on every push/PR (against the git
fork deps, so the job is self-contained and does not need `./sui`). That workflow IS the gate in CI.

## Which Sui the gate builds against (important)

`movy/Cargo.toml` has **two interchangeable dependency blocks** for the fork crates; exactly one is
active. Both list the same crates with the same features, so toggling never changes behaviour — switch
by commenting one block and uncommenting the other (search for `(A) git block` / `(B) local`).

- **(A) git block — the committed DEFAULT.** Self-contained; this is what CI and normal builds use.
- **(B) local `../sui` path block — opt-in.** Switch to it locally to validate re-applied patches
  against your *working* `./sui` **before pushing**. Switch back to (A) before committing — leaving (B)
  active would break CI/release/docker (they don't have `../sui`).

## Upgrade workflow

1. In `../sui`, rebase the fork onto the new upstream release and re-apply the 13 patches below.
2. In `movy/Cargo.toml`, switch to the **(B) local path block** (comment (A), uncomment (B)).
3. `cargo test -p movy-fork-tests`. Every failure points at a patch whose semantic was lost — see the
   matching row below. Green = the fork still behaves as movy needs.
4. Switch `movy/Cargo.toml` **back to (A) git**, push `../sui`, bump the branch/`Cargo.lock` rev if it
   changed, and let CI (`test.yaml`) re-run the gate against the pushed branch.

## Verifying a guard actually bites

A guard is only useful if it fails when the patch is gone. To confirm (or after editing a guard):
temporarily revert the single patch in `../sui` (the diffs are tiny), `cargo test -p movy-fork-tests
--test <file>`, confirm the matching test fails, then `git -C ../sui checkout -- <file>`. The
`bite` column records what was verified this way.

---

## Patch → guard map

Base: `git -C ../sui log --reverse 072a211161..HEAD`. Tests live in `tests/{tracer_hooks,test_mode,misc}.rs`.

| # | commit | semantic | guard (test) | bite |
|---|--------|----------|--------------|------|
| 1 | `8d5095459e` | `impl<T: Tracer> Tracer for &mut T` | `tracer_hooks::mut_ref_tracer_with_nonstatic_lifetime` | compile-time |
| 2 | `ecc7f94cb9` | `pub mod transfer` in the **v2** natives cut | `misc.rs` `use sui_move_natives_v2::transfer` | compile-time |
| 3 | `99dbbd14b1` | trace `Instruction` carries `Bytecode`, not `Box<String>` | `tracer_hooks::before_instruction_reports_bytecode_values` | runtime |
| 4 | `bcfc8c2f80` | testing feature: `test_scenario` natives usable in real execution | `test_mode::multi_tx_test_scenario_runs_in_execution` | runtime |
| 5 | `dc264e518c` | allow `init` to be called (disable `verify_init_not_called`) | `test_mode::init_can_be_called` | verified ↓ |
| 6 | `6d728ea5ae` | allow manual OTW (disable `verify_no_instantiations`) | `test_mode::manual_otw_can_be_instantiated` | verified ↓ |
| 7 | `dbebceb0cb` | `deep_copy` keeps `accumulator_*_totals` fields | covered by #4 (compile + run) | transitive |
| 8 | `b0dd3c8df9` | (revert Bytecode serde to wasm-only) | **superseded by #10** | n/a |
| 9 | `6450bf1b55` | `MoveTraceBuilder<'a>` / `VMTracer<'a,'b>` non-`'static` tracers | `tracer_hooks::mut_ref_tracer_with_nonstatic_lifetime` | compile-time |
| 10 | `a1681ec2be` | all `file_format` types derive serde unconditionally | `tracer_hooks::file_format_types_roundtrip_serde` | compile-time |
| 11 | `44c5372133` | `end_transaction` **extends** input_objects across `next_tx` | `test_mode::multi_tx_test_scenario_runs_in_execution` | verified ↓ |
| 12 | `21a1c5f944` | `ExecutionMode::targeted_deployment()` hook | `misc::targeted_deployment_pins_package_id` | by construction |
| 13 | `0cf08add03` | protocol-config override `warn!`→`debug!` | **non-semantic (log level)** | n/a |
| 14 | `7b0edf70af` | `install_dir` redirects build artifacts off the source tree (output-keyed build lock) | `build_isolation::install_dir_redirects_artifacts_off_source_tree` | runtime |
| 15 | `7b0edf70af` | `extra_source_files` injected into the ROOT package's **test-mode** build | `build_isolation::extra_sources_inject_into_root_test_build` | verified (control) |

`bite` legend: *compile-time* = the test crate fails to **compile** if reverted; *runtime* = a test
assertion fails; *verified* = empirically confirmed by reverting (see per-patch notes); *by
construction* = the consumer (`deploy_contract`) errors internally without the patch; *transitive* =
exercised by another patch's test; *n/a* = nothing behavioural to assert.

---

## Per-patch detail

### #1 `8d5095459e` impl Tracer for &mut Tracer
- **File:** `external-crates/move/crates/move-trace-format/src/interface.rs`
- **What:** adds `impl<T: Tracer> Tracer for &mut T`.
- **Why movy:** `exec.rs::run_tx_trace_inner` boxes a `&mut tracer` as `Box<dyn Tracer>` so the caller
  keeps ownership of its tracer. Needs `&mut T: Tracer`.
- **Guard:** `mut_ref_tracer_with_nonstatic_lifetime` builds `MoveTraceBuilder::new_with_tracer(Box::new(&mut local))`.
  Reverting makes the test crate fail to compile.

### #2 `ecc7f94cb9` allow access raw transfer
- **File:** `sui-execution/v2/sui-move-natives/src/lib.rs` (`mod transfer` → `pub mod transfer`).
- **What:** exposes the `transfer` natives module in the **v2** execution cut.
- **Why movy:** movy's raw-transfer cheat (`crates/movy-sui/src/cheats/scenario.rs`, currently
  commented out) reaches into this natives surface. NOTE: that cheat references the **latest** cut
  (`sui_move_natives_latest::transfer`), whereas this patch only made **v2** public — so today the
  patch is **latent / forward-looking**. The guard pins the v2 visibility it actually changes.
- **Guard:** a `#[allow(unused_imports)] use sui_move_natives_v2::transfer;` at the top of `misc.rs`.
  Reverting to `mod transfer;` fails to compile (E0603, module private).
- **On the next port:** if movy starts consuming the latest cut's raw transfer, move the patch (and
  this guard) to `sui_move_natives_latest`.

### #3 `99dbbd14b1` do emit bytecode
- **Files:** `move-binary-format/src/file_format.rs`, `move-trace-format/src/format.rs`.
- **What:** `TraceEvent::Instruction.instruction` changes from `Box<String>` (debug text of the opcode)
  to the actual `Bytecode`; the builder stores `instruction.clone()`.
- **Why movy:** `tracer/mod.rs::MovySuiTracerExt::before_instruction(&self, …, &Bytecode)` and every
  movy tracer (concolic, coverage, tree) consume the real `Bytecode`.
- **Guard:** `before_instruction_reports_bytecode_values` runs a function while tracing and asserts the
  captured items are real `Bytecode` variants (`Add`/`Mul`/`Ret`). If reverted to `Box<String>`,
  movy-replay would not even compile.

### #4 `bcfc8c2f80` Support testing feature
- **Files:** `sui-execution/Cargo.toml`, `latest/sui-adapter/{Cargo.toml,src/adapter.rs}`,
  `latest/sui-move-natives/{Cargo.toml, src/object_runtime/{accumulator.rs,mod.rs}, src/test_scenario.rs,
  src/transaction_context.rs}`, `crates/sui-move/src/unit_test.rs`.
- **What:** a `testing` feature that (a) makes `InMemoryTestStore` own its storage instead of a
  thread-local, (b) adds `ObjectRuntimeState::deep_copy`, (c) sets `TransactionContext.test_only = true`,
  and (d) registers the test store extension during normal execution. Together this lets
  `sui::test_scenario` natives run inside a *real* programmable transaction, not just `sui move test`.
- **Why movy:** `movy_init` and every invariant run as PTBs that drive `test_scenario`
  (`exec.rs::run_ptb_with_movy_*_gas`). movy enables the feature in `Cargo.toml`
  (`sui-execution`/`sui-adapter-latest`/`move-vm-runtime` `features = ["testing"]`).
- **Guard:** `multi_tx_test_scenario_runs_in_execution` deploys a package whose `#[test] fun` runs a
  multi-transaction `test_scenario` and executes it via the executor; success is only possible with the
  feature compiled in. (Resolved O3: the feature is *compile-time*, so it works under both `Normal` and
  `SuiFuzzMode` execution — there is no runtime "Normal rejects it" to assert.)
- **Re-apply gotcha:** the feature must stay wired through all three Cargo.tomls *and* enabled in
  `movy/Cargo.toml`, or the test_scenario natives silently revert to `test_only = false` and abort.

### #5 `dc264e518c` allow test_init
- **File:** `sui-execution/latest/sui-verifier/src/entry_points_verifier.rs` (comments out
  `verify_init_not_called`).
- **What:** lets a published module contain a call to its own `init`. Upstream's bytecode verifier
  forbids it; because Sui no longer ships `FnInfo`, the check fires even for test functions, so the fork
  disables it entirely.
- **Why movy:** movy compiles user packages in **test mode** and publishes them; test code may call
  `init`. (The Move *source* compiler only allows an explicit `init` call from `#[test_only]` code —
  hence the fixture is test-mode.)
- **Guard:** `init_can_be_called` test-mode-compiles a module whose `#[test_only]` fn calls `init`,
  makes it publishable (`movy_mock`), and deploys it; deploy succeeds only with the patch.
- **Bite:** verified — re-enabling `verify_init_not_called` in `../sui` makes this test fail at deploy.

### #6 `6d728ea5ae` allow manual otw in test_init
- **File:** `sui-execution/latest/sui-verifier/src/one_time_witness_verifier.rs` (comments out
  `verify_no_instantiations`).
- **What:** lets a published module hand-construct its one-time witness. Same `FnInfo` reasoning as #5.
- **Why movy:** test-mode packages whose test code builds an OTW by hand.
- **Guard:** `manual_otw_can_be_instantiated` test-mode-compiles a module with a single-`bool` `drop`
  OTW (`MANOTW`, name = uppercased module) and a `#[test_only]` fn that constructs it, then deploys.
  (The struct must have exactly one `bool` field or it isn't detected as an OTW candidate at all.)
- **Bite:** verified — re-enabling `verify_no_instantiations` makes this test fail at deploy.

### #7 `dbebceb0cb` 1.65.2 fields changes for support testing
- **File:** `sui-execution/latest/sui-move-natives/src/object_runtime/mod.rs` (`deep_copy` also copies
  `accumulator_merge_totals` / `accumulator_split_totals`).
- **What:** keeps `deep_copy` (added by #4) in sync with upstream's `ObjectRuntimeState` fields.
- **Why movy:** a missing field would not compile (or would drop state) inside the #4 testing path.
- **Guard:** transitively `multi_tx_test_scenario_runs_in_execution` (it runs `deep_copy` on every
  `end_transaction`). **Re-apply gotcha:** if upstream adds/removes `ObjectRuntimeState` fields, update
  `deep_copy` to match — a stale field list fails to compile.

### #8 `b0dd3c8df9` revert back wasm feature change for bytecode
- **File:** `move-binary-format/src/file_format.rs`.
- **What:** intermediate step that put `Bytecode`'s serde back behind `wasm`. **Superseded by #10**,
  which makes all of `file_format` derive serde unconditionally. No standalone guard; #10's guard covers
  the end state. When re-porting, you can squash #8 into #10.

### #9 `6450bf1b55` loosen MoveTraceBuilder lifetimes
- **Files:** `move-trace-format/src/format.rs`, `move-vm-runtime/src/{interpreter.rs,runtime.rs,tracing2/tracer.rs}`.
- **What:** `MoveTraceBuilder<'a>` with `Box<dyn Tracer + 'a>`, and `VMTracer<'a, 'b>` — so the boxed
  tracer may borrow non-`'static` local state.
- **Why movy:** `exec.rs` hands the VM a tracer that borrows local state per execution.
- **Guard:** `mut_ref_tracer_with_nonstatic_lifetime` (the boxed `&mut local` is non-`'static`).
  Reverting to `'static` fails to compile.

### #10 `a1681ec2be` derive serde
- **File:** `move-binary-format/src/file_format.rs` — every `#[cfg_attr(feature="wasm", derive(Serialize,
  Deserialize))]` becomes an unconditional `#[derive(Serialize, Deserialize)]` (index types,
  `SignatureToken`, `Bytecode`, `CompiledModule`, …).
- **Why movy:** movy serialises bytecode/modules without enabling the `wasm` feature.
- **Guard:** `file_format_types_roundtrip_serde` bcs-round-trips a `CompiledModule` and serde_json-round-
  trips a `Bytecode`. Reverting the derives fails to compile.

### #11 `44c5372133` extend input objects instead of overwriting
- **File:** `sui-execution/latest/sui-move-natives/src/test_scenario.rs` — `end_transaction` does
  `input_objects.extend(taken)` instead of `input_objects = taken.collect()`.
- **What:** the per-scenario input-object set **accumulates** across `next_tx` boundaries instead of
  being reset each transaction. That set is read in `object_runtime/mod.rs` (`transfer`, ~L302) to
  recover an object's previous owner when computing transfer effects.
- **Why movy:** multi-transaction `movy_init` scenarios (create+share in one tx, take/mutate in a later
  one) rely on objects from earlier transactions staying known.
- **Guard:** `multi_tx_test_scenario_runs_in_execution`.
- **Bite:** verified — reverting to `input_objects = taken.collect()` makes the multi-tx scenario fail
  (execution aborts/panics in `sui-adapter/src/execution_engine.rs` when an earlier transaction's
  object is no longer tracked). So the existing create+share / take+mutate / re-take scenario is a real
  guard for this patch.

### #12 `21a1c5f944` support targeted deployment
- **Files (semantic):** `sui-execution/latest/sui-adapter/src/execution_mode.rs` (new
  `ExecutionMode::targeted_deployment()` hook, default `None`), `…/programmable_transactions/execution.rs`,
  `…/static_programmable_transactions/execution/{context.rs,interpreter.rs}` (publish/upgrade use the
  target id when present), plus a threaded `<Mode>` on `Context::upgrade`.
- **Files (non-semantic):** added `tracing::warn!` diagnostics in `move_package.rs`,
  `data_store/linked_data_store.rs`, `linkage/resolution.rs`, `execution.rs`, `interpreter.rs` — log only.
- **Why movy:** `exec.rs::SuiFuzzMode::targeted_deployment` returns a per-digest target id set by
  `deploy_contract`/`upgrade_contract`, so packages can be published at a fixed id (`movy ... --deploy-at`).
- **Guard:** `targeted_deployment_pins_package_id` deploys with a pinned id and asserts the package
  lands exactly there. Without the hook the package gets a fresh id and `deploy_contract`'s own
  `deployed at != target` check returns an error — so the guard fails by construction.

### #13 `0cf08add03` avoid warning for overrides
- **Files:** `crates/sui-protocol-config-macros/src/lib.rs`, `crates/sui-protocol-config/src/lib.rs`
  (`warn!` → `debug!`).
- **What:** lowers log level for protocol-config overrides. **Non-semantic** (log noise only); no guard.
  Safe to re-apply or drop without affecting behaviour.

### #14 `7b0edf70af` output-keyed build lock + install_dir artifact redirect
- **File:** `external-crates/move/crates/move-package-alt/src/package/root_package.rs`
  (`validate_and_construct`: lock `output_path` instead of `input_path`).
- **What:** the per-package build lock and all written artifacts/lockfile follow the **output** dir
  (`install_dir`), not the source dir. When `output_path != input_path` the source tree is read-only,
  and concurrent loads of the same source with distinct output dirs no longer serialize on a shared
  input-keyed lock. When they're equal (default in-place build) behaviour is unchanged.
- **Why movy:** the audit harness runs many `movy sui test` processes over ONE shared, read-only
  audited package, each with its own `--install-dir`; without this they would write into the same
  `build/` and serialize on one lock (see `crates/movy/src/sui/env.rs` `BuildIsolationArgs`).
- **Guard:** `build_isolation::install_dir_redirects_artifacts_off_source_tree` builds a package with
  an `install_dir` and asserts `<source>/build` is absent while `<install_dir>/build` exists. (The
  concurrency property itself is not unit-testable in-process; this pins the artifact-redirection
  half the lock keying depends on.)

### #15 `7b0edf70af` extra_source_files injected into the root test build
- **Files:** `external-crates/move/crates/move-package-alt-compilation/src/build_config.rs`
  (`extra_source_files` field) + `…/compilation.rs::make_deps_for_compiler`
  (`if pkg.is_root() && test_mode { extend target with extra_source_files }`).
- **What:** extra `.move` files supplied via `BuildConfig` are compiled into the ROOT package's
  named-address scope, in **test mode only**, on top of its own `sources/`.
- **Why movy:** lets the harness compile a generated `module <target>::knowdit_spec_N { … }` into the
  audited package without copying it onto disk (`crates/movy-sui/src/compile.rs` `BuildIsolation`,
  threaded from `--extra-sources`).
- **Guard:** `build_isolation::extra_sources_inject_into_root_test_build` compiles a package whose
  on-disk `main` does `use p::injected`, where `p::injected` exists ONLY in an extra source file — the
  build succeeds iff the file was injected. Its control build (no extra sources) must fail, which is
  the bite.

---

## Adding a guard for a new patch

When a future port adds a new fork patch:
1. Find movy's consumer of the new behaviour (usually `crates/movy-replay/src/exec.rs`,
   `…/tracer/`, or `crates/movy-sui/src/compile.rs`).
2. Add a `#[test]` to the matching `tests/*.rs` that exercises that behaviour through the offline
   `harness()` and asserts the semantic (or, for type/signature changes, simply uses the new API so a
   revert fails to compile).
3. Add a row to the table above and a per-patch entry.
4. Verify it bites (revert the patch in `../sui`, confirm the test fails, restore).
