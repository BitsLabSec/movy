use move_trace_format::format::TraceEvent;
use move_vm_stack::Stack;
use movy_types::{
    error::MovyError,
    input::{FunctionIdent, InputArgument, MoveSequence},
    oracle::OracleFinding,
};
use sui_types::effects::TransactionEffects;

use crate::tracer::concolic::ConcolicState;

pub trait SuiGeneralOracle<T, S> {
    fn pre_execution(
        &mut self,
        db: &T,
        state: &mut S,
        sequence: &MoveSequence,
    ) -> Result<(), MovyError>;

    fn event(
        &mut self,
        event: &TraceEvent,
        stack: Option<&Stack>,
        symbol_stack: &ConcolicState,
        current_function: Option<FunctionIdent>,
        state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError>;

    fn done_execution(
        &mut self,
        db: &T,
        state: &mut S,
        effects: &TransactionEffects,
    ) -> Result<Vec<OracleFinding>, MovyError>;
}

impl<T, S> SuiGeneralOracle<T, S> for () {
    fn pre_execution(
        &mut self,
        _db: &T,
        _state: &mut S,
        _sequence: &MoveSequence,
    ) -> Result<(), MovyError> {
        Ok(())
    }

    fn event(
        &mut self,
        _event: &TraceEvent,
        _stack: Option<&Stack>,
        _symbol_stack: &ConcolicState,
        _current_function: Option<movy_types::input::FunctionIdent>,
        _state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        Ok(vec![])
    }

    fn done_execution(
        &mut self,
        _db: &T,
        _state: &mut S,
        _effects: &TransactionEffects,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        Ok(vec![])
    }
}

impl<T, S, O1, O2> SuiGeneralOracle<T, S> for (O1, O2)
where
    O1: SuiGeneralOracle<T, S>,
    O2: SuiGeneralOracle<T, S>,
{
    fn pre_execution(
        &mut self,
        db: &T,
        state: &mut S,
        sequence: &MoveSequence,
    ) -> Result<(), MovyError> {
        self.0.pre_execution(db, state, sequence)?;
        self.1.pre_execution(db, state, sequence)
    }

    fn event(
        &mut self,
        event: &TraceEvent,
        stack: Option<&Stack>,
        symbol_stack: &ConcolicState,
        current_function: Option<movy_types::input::FunctionIdent>,
        state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        let mut findings = vec![];
        findings.extend(self.0.event(
            event,
            stack,
            symbol_stack,
            current_function.clone(),
            state,
        )?);
        findings.extend(
            self.1
                .event(event, stack, symbol_stack, current_function, state)?,
        );
        Ok(findings)
    }

    fn done_execution(
        &mut self,
        db: &T,
        state: &mut S,
        effects: &TransactionEffects,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        let mut findings = vec![];
        findings.extend(self.0.done_execution(db, state, effects)?);
        findings.extend(self.1.done_execution(db, state, effects)?);
        Ok(findings)
    }
}

pub struct CouldDisabledOralce<O> {
    pub oracle: O,
    pub disabled: bool,
}

impl<O> CouldDisabledOralce<O> {
    pub fn new(oracle: O, disabled: bool) -> Self {
        Self { oracle, disabled }
    }
}

impl<O, T, S> SuiGeneralOracle<T, S> for CouldDisabledOralce<O>
where
    O: SuiGeneralOracle<T, S>,
{
    fn pre_execution(
        &mut self,
        db: &T,
        state: &mut S,
        sequence: &MoveSequence,
    ) -> Result<(), MovyError> {
        if self.disabled {
            return Ok(());
        }
        self.oracle.pre_execution(db, state, sequence)
    }

    fn event(
        &mut self,
        event: &TraceEvent,
        stack: Option<&Stack>,
        symbol_stack: &ConcolicState,
        current_function: Option<FunctionIdent>,
        state: &mut S,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        if self.disabled {
            return Ok(vec![]);
        }
        self.oracle
            .event(event, stack, symbol_stack, current_function, state)
    }

    fn done_execution(
        &mut self,
        db: &T,
        state: &mut S,
        effects: &TransactionEffects,
    ) -> Result<Vec<OracleFinding>, MovyError> {
        if self.disabled {
            return Ok(vec![]);
        }
        self.oracle.done_execution(db, state, effects)
    }
}
