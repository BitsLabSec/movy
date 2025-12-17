use std::fmt::Display;

use itertools::Itertools;
use log::warn;
use move_trace_format::{
    format::{Frame, TraceEvent, TraceValue},
    interface::Tracer,
};
use move_vm_stack::Stack;

#[derive(Debug, Clone)]
pub struct FrameTraced {
    pub open: Box<Frame>,
    pub subcalls: Vec<FrameTraced>,
    pub close: Option<Vec<TraceValue>>,
}

impl Display for FrameTraced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let r = self
            .close
            .as_ref()
            .map(|v| v.iter().map(|t| t.to_string()).join(","))
            .unwrap_or_default();
        f.write_fmt(format_args!(
            "{}:{}:{}{}({}){}",
            self.open.module.address().to_canonical_string(true),
            self.open.module.name().as_str(),
            self.open.function_name,
            if !self.open.type_instantiation.is_empty() {
                format!(
                    "<{}>",
                    self.open
                        .type_instantiation
                        .iter()
                        .map(|v| v.to_canonical_string(true))
                        .join(",")
                )
            } else {
                "".to_string()
            },
            self.open.parameters.iter().map(|v| v.to_string()).join(","),
            if r.is_empty() {
                "".to_string()
            } else {
                format!(" -> {}", r)
            }
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub struct TreeTraceResult {
    calls: Vec<FrameTraced>,
    call_idxs: Vec<usize>,
    evs: Vec<TraceEvent>,
}

impl TreeTraceResult {
    pub fn current_frame(&mut self) -> Option<&mut FrameTraced> {
        if self.call_idxs.is_empty() {
            return None;
        }
        let mut current = self
            .calls
            .get_mut(*self.call_idxs.first().unwrap())
            .unwrap();
        for idx in self.call_idxs.iter().skip(1) {
            current = current.subcalls.get_mut(*idx).unwrap();
        }

        Some(current)
    }
    pub fn current_calls(&mut self) -> &mut Vec<FrameTraced> {
        let mut current = &mut self.calls;
        for idx in self.call_idxs.iter() {
            current = &mut current.get_mut(*idx).unwrap().subcalls;
        }
        current
    }

    fn pprint_child(calls: &Vec<FrameTraced>, tr: &mut ptree::TreeBuilder) {
        for child in calls.iter() {
            if child.subcalls.is_empty() {
                tr.add_empty_child(child.to_string());
            } else {
                tr.begin_child(child.to_string());
                Self::pprint_child(&child.subcalls, tr);
                tr.end_child();
            }
        }
    }

    pub fn pprint_call_tree(&self) -> ptree::TreeBuilder {
        let mut tr = ptree::TreeBuilder::new("calltree".to_string());
        Self::pprint_child(&self.calls, &mut tr);
        tr
    }

    pub fn pprint(&self) -> String {
        let mut tr = self.pprint_call_tree();
        let mut buf = vec![];
        let out = std::io::Cursor::new(&mut buf);
        ptree::write_tree(&tr.build(), out).unwrap();
        String::from_utf8(buf).unwrap()
    }

    pub fn into_raw(self) -> Vec<TraceEvent> {
        self.evs
    }
}

#[derive(Debug, Clone, Default)]
pub struct TreeTracer {
    pub inner: TreeTraceResult,
}

impl TreeTracer {
    pub fn new() -> Self {
        Self {
            inner: TreeTraceResult::default(),
        }
    }

    pub fn take_inner(self) -> TreeTraceResult {
        self.inner
    }
}

impl Tracer for TreeTracer {
    fn notify(
        &mut self,
        event: &move_trace_format::format::TraceEvent,
        _writer: &mut move_trace_format::interface::Writer<'_>,
        _stack: Option<&Stack>,
    ) {
        let inner = &mut self.inner;
        inner.evs.push(event.clone());
        match event {
            TraceEvent::OpenFrame { frame, gas_left } => {
                let current = inner.current_calls();
                let idx_len = current.len();
                current.push(FrameTraced {
                    open: frame.clone(),
                    subcalls: vec![],
                    close: None,
                });
                // drop(current);
                inner.call_idxs.push(idx_len);
            }
            TraceEvent::CloseFrame {
                frame_id,
                return_,
                gas_left,
            } => {
                let current = inner.current_frame();
                if current.is_none() {
                    warn!("current frame is none when trying to close frame!?");
                } else {
                    current.unwrap().close = Some(return_.clone());
                }
                inner.call_idxs.pop();
            }
            _ => {}
        }
    }
}
