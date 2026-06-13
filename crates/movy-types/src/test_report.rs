//! Structured `movy sui test` output schema.
//!
//! Written by [`movy_fuzz::operations::sui_test::test`] when the CLI
//! is invoked with `--machine-output <path>`. The audit pipeline
//! (`knowdit-move`'s `MovyHarness::fuzz_one_existing_spec`) shells
//! out to `movy sui test --filter <name> --machine-output <json>
//! --lcov <lcov>` per synthesized harness and parses this report
//! to decide per-spec pass/fail/oracle-crash.
//!
//! Schema stability contract:
//!
//! * [`TestRunReport::SCHEMA_VERSION`] is bumped on **any** breaking
//!   change to the JSON shape (field rename, semantic change,
//!   removal of a variant). Consumers MUST check the version field
//!   and reject unknown versions with a clear error rather than
//!   silently misinterpreting.
//! * Adding a new optional field, or a new variant tagged with
//!   `#[serde(other)]`-compatible naming, is **not** a breaking
//!   change and does not bump the version.
//!
//! No internal movy-fuzz types appear in this report — only stable
//! string identifiers (function = `pkg::module::name`) and primitive
//! payloads. That way the audit consumer can match-on the JSON
//! without re-implementing FunctionIdent.

use std::path::Path;

use color_eyre::eyre::eyre;
use serde::{Deserialize, Serialize};

use crate::error::MovyError;

/// Top-level report. Written as a single JSON object to the path
/// passed via `--machine-output`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestRunReport {
    /// Schema version — see [`Self::SCHEMA_VERSION`].
    pub version: u32,
    /// One entry per target function, in the order they were run.
    /// Length matches `summary.total`.
    pub functions: Vec<FunctionResult>,
    /// Roll-up counters across `functions`.
    pub summary: Summary,
    /// Raw LCOV text rendered from this run's coverage, when a
    /// coverage map was supplied. Embedded inline so consumers
    /// get coverage in the same atomic JSON they get verdicts from —
    /// no separate file shuffle, no "did movy actually write it"
    /// race. Size is bounded by `--locals` package count and is
    /// fine for the per-spec audit pipeline; CLI users who want
    /// it on disk can still pass `--lcov <path>` and the CLI
    /// layer copies the same content out.
    pub lcov: Option<String>,
}

impl TestRunReport {
    /// Current schema version. Bump on breaking JSON shape changes.
    pub const SCHEMA_VERSION: u32 = 2;

    pub fn empty() -> Self {
        Self {
            version: Self::SCHEMA_VERSION,
            functions: Vec::new(),
            summary: Summary::default(),
            lcov: None,
        }
    }

    /// Append one function's result and fold it into the rolling
    /// summary. Single entry point so the run loop reads as one
    /// `report.record(...)` per outcome — no chance of pushing a
    /// `FunctionResult` without bumping `summary`.
    pub fn record(&mut self, function: String, outcome: Outcome) {
        self.summary.observe(&outcome);
        self.functions.push(FunctionResult { function, outcome });
    }

    /// Serialize to pretty JSON and write to `path`, creating parent
    /// directories as needed. This is the machine-output channel: the
    /// CLI exits 0 regardless of per-function outcomes, and callers
    /// decide pass/fail by inspecting [`Self::summary`] after reading
    /// this file back.
    pub fn write_machine(&self, path: &Path) -> Result<(), MovyError> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| MovyError::from(eyre!("failed to serialize test report: {e}")))?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent).map_err(|e| {
                MovyError::from(eyre!(
                    "failed to create parent directory {} for --machine-output: {e}",
                    parent.display()
                ))
            })?;
        }
        std::fs::write(path, json).map_err(|e| {
            MovyError::from(eyre!(
                "failed to write --machine-output {}: {e}",
                path.display()
            ))
        })?;
        Ok(())
    }

    /// Human-readable renderer used when `--machine-output` is NOT set.
    /// One `ok <fn>` / failure block per function followed by a summary
    /// line, matching the pre-collect-all stdout format so scripts on
    /// top of it don't break. `trace` is accepted for signature parity
    /// with the legacy renderer; full per-case traces are only available
    /// via `--machine-output` (which carries them in the JSON).
    pub fn render_human(&self, trace: bool) {
        let _ = trace;
        for entry in &self.functions {
            match &entry.outcome {
                Outcome::Ok => {
                    println!("ok {}", entry.function);
                }
                Outcome::SequenceBuildFailure => {
                    println!(
                        "FAIL {}: unable to construct a test sequence",
                        entry.function
                    );
                }
                Outcome::ExecutionFailure {
                    status_debug,
                    sequence,
                    trace: t,
                } => {
                    println!(
                        "FAIL {}: execution failed\n  status: {}\n  sequence:\n{}",
                        entry.function, status_debug, sequence
                    );
                    if let Some(t) = t.as_deref() {
                        println!("  trace:\n{t}");
                    }
                }
                Outcome::OracleCrash {
                    reason,
                    sequence,
                    trace: t,
                } => {
                    let r = reason.as_deref().unwrap_or("<no reason>");
                    println!(
                        "FAIL {}: oracle crash: {}\n  sequence:\n{}",
                        entry.function, r, sequence
                    );
                    if let Some(t) = t.as_deref() {
                        println!("  trace:\n{t}");
                    }
                }
            }
        }
        println!(
            "summary: total={} ok={} oracle_crash={} execution_failure={} sequence_build_failure={}",
            self.summary.total,
            self.summary.ok,
            self.summary.oracle_crash,
            self.summary.execution_failure,
            self.summary.sequence_build_failure,
        );
    }
}

/// Per-function execution outcome.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionResult {
    /// Fully-qualified `pkg::module::name` identifier of the test
    /// function as movy ran it.
    pub function: String,
    /// What happened. See [`Outcome`] variants.
    pub outcome: Outcome,
}

/// Outcome of running one target function. Tagged enum on `kind`
/// so a stable parser can match a single string field rather than
/// inspecting the shape of the payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    /// Function ran to completion and emitted no oracle violation.
    Ok,
    /// `append_function` could not construct a valid call sequence
    /// for this function (e.g. its parameter types could not be
    /// fuzzed-from-scratch under the supplied `--object-mapping` /
    /// `--test-ty`). No transaction was attempted.
    SequenceBuildFailure,
    /// The PTB ran but the transaction status was non-`Success`.
    /// `status_debug` carries the `Debug` form of
    /// [`sui_types::execution_status::ExecutionStatus`] so the
    /// caller can match on the abort code without us promising a
    /// structured copy of that enum.
    ExecutionFailure {
        status_debug: String,
        sequence: String,
        trace: Option<String>,
    },
    /// Transaction status was `Success` but a `movy::oracle::Crash`
    /// event was emitted — the test's pre/post oracles reported an
    /// invariant violation.
    OracleCrash {
        /// Decoded `reason` field of the `movy::oracle::Crash`
        /// event payload, when it parses. Falls back to `None` if
        /// the event carried no readable reason.
        reason: Option<String>,
        sequence: String,
        trace: Option<String>,
    },
}

/// Roll-up counters. Matches `functions.iter().filter(...)` over
/// each [`Outcome`] variant, so callers that only need a quick
/// pass/fail tally don't have to iterate the per-function list.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Summary {
    /// Total functions in this run (== `functions.len()`).
    pub total: usize,
    pub ok: usize,
    pub sequence_build_failure: usize,
    pub execution_failure: usize,
    pub oracle_crash: usize,
}

impl Summary {
    /// Increment the right counter for `outcome` plus the running
    /// `total`. Callers fold this over each function result.
    pub fn observe(&mut self, outcome: &Outcome) {
        self.total += 1;
        match outcome {
            Outcome::Ok => self.ok += 1,
            Outcome::SequenceBuildFailure => self.sequence_build_failure += 1,
            Outcome::ExecutionFailure { .. } => self.execution_failure += 1,
            Outcome::OracleCrash { .. } => self.oracle_crash += 1,
        }
    }

    /// True iff every function in the run produced `Outcome::Ok`.
    /// `MovyHarness` treats this as the green-path predicate.
    pub fn all_ok(&self) -> bool {
        self.total > 0 && self.ok == self.total
    }

    /// True iff at least one function reported a violation
    /// (oracle crash) — the audit pipeline's primary signal that a
    /// candidate spec has bitten.
    pub fn any_oracle_crash(&self) -> bool {
        self.oracle_crash > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_observes_each_variant() {
        let mut s = Summary::default();
        s.observe(&Outcome::Ok);
        s.observe(&Outcome::OracleCrash {
            reason: Some("x".into()),
            sequence: "seq".into(),
            trace: None,
        });
        s.observe(&Outcome::ExecutionFailure {
            status_debug: "Abort(_)".into(),
            sequence: "seq".into(),
            trace: None,
        });
        s.observe(&Outcome::SequenceBuildFailure);
        assert_eq!(s.total, 4);
        assert_eq!(s.ok, 1);
        assert_eq!(s.oracle_crash, 1);
        assert_eq!(s.execution_failure, 1);
        assert_eq!(s.sequence_build_failure, 1);
        assert!(!s.all_ok());
        assert!(s.any_oracle_crash());
    }

    #[test]
    fn all_ok_requires_nonempty() {
        let s = Summary::default();
        assert!(!s.all_ok(), "0/0 is not all-ok — we want runs that emit no work to look distinct");
    }

    #[test]
    fn outcome_json_tag_is_kind_snake_case() {
        // Stable contract: the consumer parser matches on the
        // `kind` string field, not on the JSON object shape.
        let ok = serde_json::to_value(Outcome::Ok).unwrap();
        assert_eq!(ok["kind"], "ok");

        let sbf = serde_json::to_value(Outcome::SequenceBuildFailure).unwrap();
        assert_eq!(sbf["kind"], "sequence_build_failure");

        let crash = serde_json::to_value(Outcome::OracleCrash {
            reason: Some("invariant_X".into()),
            sequence: "seq()".into(),
            trace: None,
        })
        .unwrap();
        assert_eq!(crash["kind"], "oracle_crash");
        assert_eq!(crash["reason"], "invariant_X");
    }

    #[test]
    fn report_roundtrips_through_json() {
        let mut r = TestRunReport::empty();
        r.record("pkg::m::test_a".into(), Outcome::Ok);
        r.lcov = Some("SF:pkg/m.move\nend_of_record\n".into());

        let s = serde_json::to_string(&r).unwrap();
        let back: TestRunReport = serde_json::from_str(&s).unwrap();
        assert_eq!(back.version, TestRunReport::SCHEMA_VERSION);
        assert_eq!(back.functions.len(), 1);
        assert_eq!(back.functions[0].function, "pkg::m::test_a");
        assert!(back.summary.all_ok());
        assert!(back.lcov.as_deref().unwrap().contains("end_of_record"));
    }

    #[test]
    fn record_keeps_summary_in_sync_with_functions() {
        let mut r = TestRunReport::empty();
        r.record("a".into(), Outcome::Ok);
        r.record("b".into(), Outcome::SequenceBuildFailure);
        assert_eq!(r.functions.len(), 2);
        assert_eq!(r.summary.total, 2);
        assert_eq!(r.summary.ok, 1);
        assert_eq!(r.summary.sequence_build_failure, 1);
    }
}
