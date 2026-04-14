use std::{borrow::Cow, collections::BTreeMap};

use color_eyre::eyre::eyre;
use libafl::{executors::ExitKind, observers::StdMapObserver};
use libafl_bolts::tuples::{Handle, MatchName, MatchNameRef};
use move_binary_format::{CompiledModule, file_format::Bytecode};
use move_trace_format::format::{Effect, TraceEvent, TraceValue};
use movy_types::{
    abi::MoveModuleId, error::MovyError, input::FunctionIdent, oracle::OracleFinding,
};
use sui_types::{base_types::ObjectID, storage::BackingPackageStore};
use tracing::warn;

use crate::tracer::{
    MovySuiTracerExt,
    concolic::ConcolicState,
    extra::InstructionExtraInformation,
    op::{CastLog, CmpLog, CmpOp, Log, Magic, ShlLog},
    oracle::SuiGeneralOracle,
    state::TraceState,
};

#[derive(Default, Debug)]
pub struct PackageResolvedCache {
    pub packages: BTreeMap<ObjectID, BTreeMap<MoveModuleId, CompiledModule>>,
}

impl PackageResolvedCache {
    pub fn module_ref(
        &self,
        module_id: &MoveModuleId,
        package_id: &ObjectID,
    ) -> Option<&CompiledModule> {
        self.packages.get(package_id).and_then(|v| v.get(module_id))
    }
}
pub struct PackageResolver<T> {
    pub db: T,
    pub cache: PackageResolvedCache,
}

impl<T: BackingPackageStore> PackageResolver<T> {
    pub fn may_load_package(&mut self, package_id: &ObjectID) -> Result<(), MovyError> {
        if !self.cache.packages.contains_key(package_id) {
            if let Some(package) = self.db.get_package_object(package_id)? {
                for md in package.move_package().serialized_module_map().values() {
                    let module = CompiledModule::deserialize_with_defaults(md)?;
                    let id = module.self_id();
                    self.cache
                        .packages
                        .entry(*package_id)
                        .or_default()
                        .insert(id.into(), module);
                }
                Ok(())
            } else {
                Err(eyre!("package {} missing from the db", package_id).into())
            }
        } else {
            Ok(())
        }
    }
}

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
                tracing::trace!("Hit a coverage at {} within package hash {}", pc, pkg);
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
        tracing::trace!("Calling package {} to {}", package, pkg);
        self.packages.push(pkg);
        self.had_br = true;
    }

    pub fn call_end_package(&mut self) {
        let pkg = self.packages.pop();
        tracing::trace!("Leaving {:?}", &pkg);
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
    O: SuiGeneralOracle<S>,
    OT: MatchNameRef + MatchName,
{
    // (package_id, module_id, module_name, function_name)
    current_functions: Vec<(ObjectID, FunctionIdent)>,
    coverage: CoverageTracer<'a, OT>,
    state: &'s mut S,
    outcome: TraceOutcome,
    oracles: &'s mut O,
    skip_concolic: bool,
    pub resolver: PackageResolver<T>, // Do note db is not updated during execution
}

impl<'a, 's, S, O, T, OT> SuiFuzzTracer<'a, 's, S, O, T, OT>
where
    O: SuiGeneralOracle<S>,
    OT: MatchNameRef + MatchName,
    T: BackingPackageStore,
{
    pub fn new(
        ob: &'a mut OT,
        state: &'s mut S,
        oracles: &'s mut O,
        ob_name: &'static str,
        resolver: PackageResolver<T>,
    ) -> Self {
        Self {
            current_functions: vec![],
            coverage: CoverageTracer::new(ob, ob_name),
            state,
            outcome: TraceOutcome::new(),
            oracles,
            skip_concolic: false,
            resolver,
        }
    }

    pub fn outcome(self) -> TraceOutcome {
        self.outcome
    }

    fn bin_ops(stack: &[TraceValue]) -> Result<(Magic, Magic), MovyError> {
        if stack.len() < 2 {
            return Err(eyre!("stack less than 2?!").into());
        }
        let lhs = Magic::try_from(&stack[stack.len() - 2])?;
        let rhs = Magic::try_from(&stack[stack.len() - 1])?;
        Ok((lhs, rhs))
    }

    fn before_instruction(
        &mut self,
        state: &TraceState,
        _tys: &Vec<sui_types::TypeTag>,
        pc: u16,
        _gas_left: u64,
        instruction: &Bytecode,
    ) -> Result<(), MovyError> {
        if let Some((_, current_function)) = self.current_functions.last() {
            let findings = self.oracles.before_instruction(
                pc,
                instruction,
                state,
                &self.outcome.concolic,
                current_function,
                self.state,
            )?;
            if !findings.is_empty() {
                self.outcome.verdict = ExitKind::Crash;
            }
            for info in findings {
                self.outcome.findings.push(info);
            }
        } else {
            tracing::warn!("no current function in before_instruction?!");
        };

        let extra = self.current_functions.last().and_then(|(pkg, md)| {
            InstructionExtraInformation::from_resolver(
                instruction,
                &self.resolver.cache,
                pkg,
                &md.0,
            )
        });

        let constraint = if !self.skip_concolic {
            self.outcome
                .concolic
                .on_before_instruction_inner(pc, instruction, state, extra)
        } else {
            tracing::trace!("concolic is skipped...");
            None
        };
        self.coverage.may_do_coverage(pc);
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
            | Bytecode::Eq => match Self::bin_ops(&state.operand_stack) {
                Ok((lhs, rhs)) => {
                    if let Some((_, current_function)) = self.current_functions.first() {
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
                    if !matches!(instruction, Bytecode::Eq) && !matches!(instruction, Bytecode::Neq)
                    {
                        warn!("Can not track cmplog due to {}", e);
                    }
                }
            },
            Bytecode::Shl => match Self::bin_ops(&state.operand_stack) {
                Ok((lhs, rhs)) => {
                    if let Some((_, current_function)) = self.current_functions.first() {
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
                    if !matches!(instruction, Bytecode::Eq) && !matches!(instruction, Bytecode::Neq)
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
                if let Some(lhs) = state.operand_stack.last() {
                    let lhs: Magic = Magic::try_from(lhs)?;
                    if let Some((_, current_function)) = self.current_functions.first() {
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
        Ok(())
    }
}

fn format_event(ev: &TraceEvent) -> String {
    match ev {
        TraceEvent::CloseFrame {
            frame_id,
            return_: _,
            gas_left: _,
        } => {
            format!("CloseFrame(id={})", frame_id)
        }
        TraceEvent::OpenFrame { frame, gas_left: _ } => {
            format!(
                "OpenFrame(id={}, package={}, target={}::{})",
                frame.frame_id,
                frame.version_id,
                frame.module.to_canonical_string(true),
                &frame.function_name,
            )
        }
        TraceEvent::Instruction {
            type_parameters: _,
            pc,
            gas_left: _,
            instruction,
        } => {
            format!("Instruction(pc={}, instruction={:?})", *pc, instruction)
        }
        TraceEvent::Effect(e) => {
            format!("Effect({})", e)
        }
        TraceEvent::External(ext) => {
            format!("External({})", ext)
        }
        _ => "-".to_string(),
    }
}

impl<'a, 's, S, O, T, OT> MovySuiTracerExt for SuiFuzzTracer<'a, 's, S, O, T, OT>
where
    O: SuiGeneralOracle<S>,
    OT: MatchNameRef + MatchName,
    T: BackingPackageStore,
{
    fn on_raw_event(&mut self, state: &TraceState, ev: &TraceEvent) -> bool {
        tracing::trace!(
            "Raw event: {}, trace stack: {}",
            format_event(ev),
            state.operand_stack_debug()
        );
        self.skip_concolic = !self.outcome.concolic.on_raw_event_inner(ev, state); // ignore return values

        true
    }

    fn on_effect(&mut self, _state: &TraceState, effect: &Box<Effect>) {
        if let Effect::ExecutionError(e) = effect.as_ref()
            && e.contains("!! TRACING ERROR !!")
        {
            warn!("Receive an error from tracing: {}", e);
        }
    }

    fn on_move_call(&mut self, _state: &TraceState) {
        if !self.skip_concolic {
            self.outcome.concolic.on_move_call_inner();
        }
    }

    fn open_frame(
        &mut self,
        state: &TraceState,
        frame: &Box<move_trace_format::format::Frame>,
        _gas_left: u64,
    ) {
        if let Err(e) = self.resolver.may_load_package(&frame.version_id.into()) {
            tracing::error!("fail to load package: {}", e);
        }
        if !self.skip_concolic {
            self.outcome.concolic.on_open_frame_inner(frame, state);
        }
        let package = format!(
            "{}:{}:{}",
            frame.module.address().to_canonical_string(true),
            frame.module.name(),
            &frame.function_name
        );
        tracing::debug!("Entering {}", &package);
        if frame.function_name.contains("flash_loan") {
            tracing::info!("entering flash-loan frame {}", package);
        }
        self.current_functions.push((
            frame.version_id.into(),
            FunctionIdent::new(
                &(*frame.module.address()).into(),
                &frame.module.name().to_string(),
                &frame.function_name.clone(),
            ),
        ));
        self.coverage.call_package(package);
    }

    fn close_frame(
        &mut self,
        state: &TraceState,
        _frame_id: move_trace_format::format::TraceIndex,
        _return_: &Vec<move_trace_format::format::TraceValue>,
        _gas_left: u64,
    ) {
        if !self.skip_concolic {
            self.outcome.concolic.on_close_frame_inner(state);
        }
        self.coverage.call_end_package();
        self.current_functions.pop();
    }

    fn before_instruction(
        &mut self,
        state: &TraceState,
        tys: &Vec<sui_types::TypeTag>,
        pc: u16,
        gas_left: u64,
        instruction: &Bytecode,
    ) {
        if let Err(e) = self.before_instruction(state, tys, pc, gas_left, instruction) {
            tracing::warn!("we have an error during tracing: {}", e);
        }
    }
}
