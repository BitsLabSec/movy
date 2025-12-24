use std::{borrow::Cow, collections::BTreeMap, marker::PhantomData};

use color_eyre::eyre::{OptionExt, eyre};
use libafl::{executors::ExitKind, observers::StdMapObserver};
use libafl_bolts::tuples::{Handle, MatchName, MatchNameRef};
use log::{trace, warn};
use move_binary_format::file_format::Bytecode;
use move_trace_format::{
    format::{Effect, TraceEvent},
    interface::Tracer,
};
use move_vm_stack::Stack;
use move_vm_types::values::IntegerValue;
use movy_types::{error::MovyError, input::FunctionIdent, oracle::OracleFinding};

use crate::tracer::{
    concolic::ConcolicState,
    op::{CastLog, CmpLog, CmpOp, Log, Magic, ShlLog},
    oracle::SuiGeneralOracle,
};

#[derive(Debug)]
pub struct CoverageTracer<'a, OT>
where
    OT: MatchNameRef + MatchName,
{
    pub packages: Vec<u64>,
    pub had_br: bool,
    pub prev: u64,
    pub ob: &'a mut OT,
    pub ob_name: &'static str,
}

impl<'a, OT> CoverageTracer<'a, OT>
where
    OT: MatchNameRef + MatchName,
{
    pub fn new(ob: &'a mut OT, ob_name: &'static str) -> Self {
        Self {
            packages: vec![],
            had_br: false,
            prev: 0,
            ob,
            ob_name,
        }
    }
    fn hash_package(package: &[u8]) -> u64 {
        libafl_bolts::hash_std(package)
    }

    fn hit_cov(&mut self, pc: u16) {
        let handle: Handle<StdMapObserver<'static, u8, false>> =
            Handle::new(Cow::Borrowed(self.ob_name));
        if let Some(map) = self.ob.get_mut(&handle) {
            if !map.is_empty() {
                let pkg = self.packages.last().expect("stack empty?!");
                log::trace!("Hit a coverage at {} within package hash {}", pc, pkg);
                let pc = pc as u64;
                let pc = (pc >> 4) ^ (pc << 8) ^ *pkg;
                let len = map.len() as u64;
                let hit = ((self.prev ^ pc) % len) as usize;
                self.prev = pc;

                map[hit] = map[hit].saturating_add(1);
            }
        } else {
            warn!("Coverage observer {} not found in tuple", self.ob_name);
        }
    }

    pub fn call_package(&mut self, package: String) {
        let pkg = Self::hash_package(package.as_bytes());
        log::trace!("Calling package {} to {}", package, pkg);
        self.packages.push(pkg);
        self.had_br = true;
    }

    pub fn call_end_package(&mut self) {
        let pkg = self.packages.pop();
        log::trace!("Leaving {:?}", &pkg);
        self.had_br = true;
    }

    pub fn will_branch(&mut self) {
        self.had_br = true;
    }

    pub fn may_do_coverage(&mut self, pc: u16) {
        if self.had_br {
            self.had_br = false;
            self.hit_cov(pc);
        }
    }
}

#[derive(Debug)]
pub struct TraceOutcome {
    pub pending_error: Option<MovyError>,
    pub logs: BTreeMap<FunctionIdent, Vec<Log>>,
    pub verdict: ExitKind,
    pub findings: Vec<OracleFinding>,
    pub concolic: ConcolicState,
}

impl Default for TraceOutcome {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceOutcome {
    pub fn new() -> Self {
        Self {
            pending_error: None,
            logs: BTreeMap::new(),
            verdict: ExitKind::Ok,
            findings: vec![],
            concolic: ConcolicState::default(),
        }
    }
}

pub struct SuiFuzzTracer<'a, 's, S, O, T, OT>
where
    O: SuiGeneralOracle<T, S>,
    OT: MatchNameRef + MatchName,
{
    current_functions: Vec<FunctionIdent>,
    coverage: CoverageTracer<'a, OT>,
    state: &'s mut S,
    outcome: TraceOutcome,
    oracles: &'s mut O,
    ph: PhantomData<T>,
}

impl<'a, 's, S, O, T, OT> SuiFuzzTracer<'a, 's, S, O, T, OT>
where
    O: SuiGeneralOracle<T, S>,
    OT: MatchNameRef + MatchName,
{
    pub fn new(
        ob: &'a mut OT,
        state: &'s mut S,
        oracles: &'s mut O,
        ob_name: &'static str,
    ) -> Self {
        Self {
            current_functions: vec![],
            coverage: CoverageTracer::new(ob, ob_name),
            state,
            outcome: TraceOutcome::new(),
            oracles,
            ph: PhantomData,
        }
    }

    pub fn outcome(self) -> TraceOutcome {
        self.outcome
    }

    fn bin_ops(stack: Option<&Stack>) -> Result<(Magic, Magic), MovyError> {
        if let Some(stack) = stack {
            let stack_len = stack.value.len();
            let rhs = stack
                .value
                .get(stack_len - 1)
                .ok_or_else(|| eyre!("stack less than 2?!"))?
                .copy_value()?
                .value_as::<IntegerValue>()?
                .into();
            let lhs = stack
                .value
                .get(stack_len - 2)
                .ok_or_else(|| eyre!("stack less than 2?!"))?
                .copy_value()?
                .value_as::<IntegerValue>()?
                .into();
            Ok((lhs, rhs))
        } else {
            Err(eyre!("we need two values on top of stack but get none...").into())
        }
    }

    pub fn notify_event(
        &mut self,
        event: &TraceEvent,
        stack: Option<&move_vm_stack::Stack>,
    ) -> Result<(), MovyError> {
        let oracle_vulns = self.oracles.event(
            event,
            stack,
            &self.outcome.concolic,
            self.current_functions.last(),
            self.state,
        )?;
        if !oracle_vulns.is_empty() {
            self.outcome.verdict = ExitKind::Crash;
        }
        for info in oracle_vulns {
            self.outcome.findings.push(info);
        }
        let constraint = self.outcome.concolic.notify_event(event, stack);
        trace!("Tracing event: {:?}", event);
        match event {
            TraceEvent::OpenFrame { frame, gas_left: _ } => {
                let package = format!(
                    "{}:{}:{}",
                    frame.module.address().to_canonical_string(true),
                    frame.module.name(),
                    &frame.function_name
                );
                self.current_functions.push(FunctionIdent::new(
                    &(*frame.module.address()).into(),
                    &frame.module.name().to_string(),
                    &frame.function_name.clone(),
                ));
                self.coverage.call_package(package);
            }
            TraceEvent::CloseFrame {
                frame_id: _,
                return_: _,
                gas_left: _,
            } => {
                self.coverage.call_end_package();
                self.current_functions.pop();
            }
            TraceEvent::BeforeInstruction {
                type_parameters: _,
                pc,
                gas_left: _,
                instruction,
                extra: _,
            } => {
                // if let Some(metrics) = self.state.eval_metrics_mut() {
                //     if let Some(current) = self.current_functions.last() {
                //         metrics.on_pc(&current.0, &current.1, *pc);
                //     } else {
                //         warn!("no current function when before instruction at {}", pc);
                //     }
                // }
                self.coverage.may_do_coverage(*pc);
                match instruction {
                    Bytecode::BrFalse(_)
                    | Bytecode::BrTrue(_)
                    | Bytecode::Branch(_)
                    | Bytecode::VariantSwitch(_) => {
                        self.coverage.will_branch();
                    }
                    Bytecode::Lt
                    | Bytecode::Le
                    | Bytecode::Ge
                    | Bytecode::Gt
                    | Bytecode::Neq
                    | Bytecode::Eq => match Self::bin_ops(stack) {
                        Ok((lhs, rhs)) => {
                            if let Some(current_function) = self.current_functions.first() {
                                let op = CmpOp::try_from(instruction)?;
                                self.outcome
                                    .logs
                                    .entry(current_function.clone())
                                    .or_default()
                                    .push(Log::CmpLog(CmpLog {
                                        lhs,
                                        rhs,
                                        op,
                                        constraint,
                                    }));
                            } else {
                                warn!("Fail to track cmplog because of no current function")
                            }
                        }
                        Err(e) => {
                            if !matches!(instruction, Bytecode::Eq)
                                && !matches!(instruction, Bytecode::Neq)
                            {
                                warn!("Can not track cmplog due to {}", e);
                            }
                        }
                    },
                    Bytecode::Shl => match Self::bin_ops(stack) {
                        Ok((lhs, rhs)) => {
                            if let Some(current_function) = self.current_functions.first() {
                                self.outcome
                                    .logs
                                    .entry(current_function.clone())
                                    .or_default()
                                    .push(Log::ShlLog(ShlLog {
                                        lhs,
                                        rhs,
                                        constraint,
                                    }));
                            } else {
                                warn!("Fail to track cmplog because of no current function")
                            }
                        }
                        Err(e) => {
                            if !matches!(instruction, Bytecode::Eq)
                                && !matches!(instruction, Bytecode::Neq)
                            {
                                warn!("Can not track cmplog due to {}", e);
                            }
                        }
                    },
                    Bytecode::CastU8
                    | Bytecode::CastU16
                    | Bytecode::CastU32
                    | Bytecode::CastU64
                    | Bytecode::CastU128 => {
                        if let Some(Some(lhs)) = stack.map(|s| s.value.last()) {
                            let lhs: Magic = lhs.copy_value()?.value_as::<IntegerValue>()?.into();
                            if let Some(current_function) = self.current_functions.first() {
                                self.outcome
                                    .logs
                                    .entry(current_function.clone())
                                    .or_default()
                                    .push(Log::CastLog(CastLog { lhs, constraint }));
                            } else {
                                warn!("Fail to track castlog because of no current function")
                            }
                        } else {
                            warn!("Can not track castlog due to stack empty");
                        }
                    }

                    _ => {}
                }
            }
            TraceEvent::Effect(e) => {
                if let Effect::ExecutionError(e) = e.as_ref()
                    && e.contains("!! TRACING ERROR !!")
                {
                    warn!("Receive an error from tracing: {}", e);
                }
            }
            _ => {}
        }

        Ok(())
    }
}

impl<'a, 's, S, O, T, OT> Tracer for SuiFuzzTracer<'a, 's, S, O, T, OT>
where
    O: SuiGeneralOracle<T, S>,
    OT: MatchNameRef + MatchName,
{
    fn notify(
        &mut self,
        event: &TraceEvent,
        _writer: &mut move_trace_format::interface::Writer<'_>,
        stack: Option<&move_vm_stack::Stack>,
    ) {
        if let Err(e) = self.notify_event(event, stack) {
            warn!("Error during tracing: {}", e);
        }
    }
}
