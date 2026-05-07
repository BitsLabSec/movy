use std::{cell::RefCell, collections::BTreeSet, rc::Rc};

use move_binary_format::file_format::Bytecode;
use move_trace_format::format::{Frame, TraceIndex, TraceValue};
use movy_sui::lcov::BytecodeLocation;

use crate::tracer::{MovySuiTracerExt, state::TraceState};

#[derive(Clone, Default)]
pub struct LineCoverageCollector {
    inner: Rc<RefCell<BTreeSet<BytecodeLocation>>>,
}

impl LineCoverageCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tracer(&self) -> LineCoverageTracer {
        LineCoverageTracer {
            hits: self.inner.clone(),
            frames: Vec::new(),
        }
    }

    pub fn hits(&self) -> BTreeSet<BytecodeLocation> {
        self.inner.borrow().clone()
    }
}

pub struct LineCoverageTracer {
    hits: Rc<RefCell<BTreeSet<BytecodeLocation>>>,
    frames: Vec<BytecodeLocation>,
}

impl MovySuiTracerExt for LineCoverageTracer {
    fn open_frame(&mut self, _state: &TraceState, frame: &Box<Frame>, _gas_left: u64) {
        self.frames.push(BytecodeLocation {
            module: frame.module.clone(),
            function: frame.binary_member_index,
            pc: 0,
        });
    }

    fn close_frame(
        &mut self,
        _state: &TraceState,
        _frame_id: TraceIndex,
        _return_: &Vec<TraceValue>,
        _gas_left: u64,
    ) {
        self.frames.pop();
    }

    fn before_instruction(
        &mut self,
        _state: &TraceState,
        _tys: &Vec<sui_types::TypeTag>,
        pc: u16,
        _gas_left: u64,
        _instruction: &Bytecode,
    ) {
        let Some(current) = self.frames.last() else {
            return;
        };
        self.hits.borrow_mut().insert(BytecodeLocation {
            module: current.module.clone(),
            function: current.function,
            pc,
        });
    }
}
