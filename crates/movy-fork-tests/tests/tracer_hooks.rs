//! Guards for the tracer / "BeforeInstruction hook" fork patches.
//!
//! Covered commits (see FORK_TESTS.md):
//!   #1  8d5095459e — `impl<T: Tracer> Tracer for &mut T`
//!   #3  99dbbd14b1 — `TraceEvent::Instruction.instruction` carries a `Bytecode` (not a `String`)
//!   #9  6450bf1b55 — `MoveTraceBuilder<'a>` / `Box<dyn Tracer + 'a>` accept a non-`'static` tracer
//!   #10 a1681ec2be — `file_format` types (incl. `CompiledModule`, `Bytecode`) derive serde

use move_binary_format::file_format::Bytecode;
use move_trace_format::{
    format::{MoveTraceBuilder, TraceEvent},
    interface::{Tracer, Writer},
};
use movy_fork_tests::harness;
use movy_replay::tracer::{MovySuiTracerExt, state::TraceState};
use sui_types::TypeTag;

// ----- #10: unconditional serde on file_format types -----

/// A trivial counting tracer over the *raw* move-trace `Tracer` trait.
#[derive(Default)]
struct CountingTracer {
    notifications: usize,
}

impl Tracer for CountingTracer {
    fn notify(&mut self, _event: &TraceEvent, _writer: Writer<'_>) -> bool {
        self.notifications += 1;
        false
    }
}

/// #1 + #9: a `&mut` local tracer (non-`'static`) can be handed to `MoveTraceBuilder` as a boxed
/// trait object. This compiles **only** with `impl Tracer for &mut T` (#1) and the loosened
/// `MoveTraceBuilder<'a>` / `Box<dyn Tracer + 'a>` lifetimes (#9). `run_tx_trace` in
/// `movy-replay/src/exec.rs` relies on exactly this construction.
#[test]
fn mut_ref_tracer_with_nonstatic_lifetime() {
    let mut local = CountingTracer::default();

    // `Box::new(&mut local)` is `Box<&mut CountingTracer>`; coercing it to the `Box<dyn Tracer + 'a>`
    // parameter requires both fork patches. If either is reverted this fails to compile.
    let builder = MoveTraceBuilder::new_with_tracer(Box::new(&mut local));
    drop(builder); // ends the `&mut local` borrow held by the boxed trait object

    // The `&mut` borrow has ended, so `local` is usable again — proving the `&mut T: Tracer` path.
    assert_eq!(local.notifications, 0);
}

/// #10: `CompiledModule` and `Bytecode` derive `Serialize`/`Deserialize` unconditionally (not only
/// under the `wasm` feature). A bcs round-trip of a real compiled module and a serde_json round-trip
/// of a `Bytecode` value both fail to *compile* if the derives are reverted.
#[test]
fn file_format_types_roundtrip_serde() {
    // A real compiled module, obtained through movy's compile API (no executor needed).
    let pkg = movy_sui::compile::SuiCompiledPackage::build_quick(
        "serde_pkg",
        "serde_mod",
        r#"module serde_pkg::serde_mod;
public fun add(a: u64, b: u64): u64 { a + b }
"#,
    )
    .expect("compile module for serde test");
    let module = pkg
        .into_deployment()
        .0
        .into_iter()
        .next()
        .expect("at least one module");

    // CompiledModule: serde round-trip via bcs.
    let bytes = bcs::to_bytes(&module).expect("CompiledModule must derive Serialize (#10)");
    let back: move_binary_format::file_format::CompiledModule =
        bcs::from_bytes(&bytes).expect("CompiledModule must derive Deserialize (#10)");
    assert_eq!(module, back, "CompiledModule serde round-trip must be lossless");

    // Bytecode: serde round-trip via serde_json.
    let op = Bytecode::LdU64(0xDEAD_BEEF);
    let json = serde_json::to_string(&op).expect("Bytecode must derive Serialize (#10)");
    let op_back: Bytecode =
        serde_json::from_str(&json).expect("Bytecode must derive Deserialize (#10)");
    assert_eq!(op, op_back, "Bytecode serde round-trip must be lossless");
}

// ----- #3: the trace carries real Bytecode instructions -----

/// Captures the `Bytecode` instructions reported through the per-instruction hook.
#[derive(Default)]
struct InstructionCapture {
    ops: Vec<Bytecode>,
}

impl MovySuiTracerExt for InstructionCapture {
    fn on_raw_event(&mut self, _state: &TraceState, _ev: &TraceEvent) -> bool {
        // Returning `true` is required for the typed per-event callbacks below to fire.
        true
    }

    fn before_instruction(
        &mut self,
        _state: &TraceState,
        _tys: &Vec<TypeTag>,
        _pc: u16,
        _gas_left: u64,
        instruction: &Bytecode,
    ) {
        self.ops.push(instruction.clone());
    }
}

/// #3 (+ #1/#9 at runtime): executing a function while tracing yields `TraceEvent::Instruction`
/// events whose `instruction` is a real `Bytecode` enum value. Upstream carried a `Box<String>`
/// (the opcode's debug text), which movy's `before_instruction(&Bytecode)` consumer could not use.
#[test]
fn before_instruction_reports_bytecode_values() {
    let mut h = harness();
    let pkg = h
        .compile(
            "work",
            "work",
            r#"module work::work;
public fun work(a: u64, b: u64): u64 {
    let c = a + b;
    let d = c * 2;
    d
}
"#,
        )
        .expect("compile work package");
    let (pkg_id, _cap) = h.deploy(pkg).expect("deploy work package");

    let ptb = h
        .call_pure_args(pkg_id, "work", "work", vec![], &[3u64, 4u64])
        .expect("build work call ptb");

    let (tracer, ok) = h
        .run_testing(ptb, Some(InstructionCapture::default()))
        .expect("traced execution should not error");
    assert!(ok, "work() execution should succeed");

    let ops = tracer.expect("tracer is returned").ops;
    assert!(
        !ops.is_empty(),
        "the trace must contain per-instruction events (#3)"
    );
    // The captured items are typed `Bytecode`; assert real arithmetic/return opcodes are present,
    // which is only possible because the trace event carries the enum, not a formatted string.
    assert!(
        ops.iter()
            .any(|b| matches!(b, Bytecode::Add | Bytecode::Mul | Bytecode::Ret)),
        "expected real arithmetic/return Bytecode variants, got: {ops:?}"
    );
}
