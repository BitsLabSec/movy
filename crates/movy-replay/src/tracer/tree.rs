use std::fmt::Display;

use itertools::Itertools;
use move_core_types::account_address::AccountAddress;
use move_trace_format::format::{Effect, Frame, TraceEvent, TraceValue};
use tracing::warn;

use crate::tracer::{MovySuiTracerExt, state::TraceState};

#[derive(Debug, Clone)]
pub struct FrameTraced {
    pub open: Box<Frame>,
    pub subcalls: Vec<FrameTraced>,
    pub close: Option<Vec<TraceValue>>,
}

impl Display for FrameTraced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.full_label())
    }
}

impl FrameTraced {
    fn type_instantiation_suffix(&self) -> String {
        if self.open.type_instantiation.is_empty() {
            String::new()
        } else {
            format!(
                "<{}>",
                self.open
                    .type_instantiation
                    .iter()
                    .map(|v| v.to_canonical_string(true))
                    .join(",")
            )
        }
    }

    fn full_label(&self) -> String {
        let returns = self
            .close
            .as_ref()
            .map(|v| v.iter().map(|t| t.to_string()).join(","))
            .unwrap_or_default();
        format!(
            "{}::{}::{}{}({}){}",
            self.open.module.address().to_canonical_string(true),
            self.open.module.name().as_str(),
            self.open.function_name,
            self.type_instantiation_suffix(),
            self.open.parameters.iter().map(|v| v.to_string()).join(","),
            if returns.is_empty() {
                String::new()
            } else {
                format!(" -> {}", returns)
            }
        )
    }

    fn name_only_label(&self) -> String {
        format!(
            "{}::{}::{}",
            self.open.module.address().to_canonical_string(true),
            self.open.module.name().as_str(),
            self.open.function_name,
        )
    }

    fn is_framework_call(&self) -> bool {
        self.open.module.address() == &AccountAddress::ONE
            || self.open.module.address() == &AccountAddress::TWO
    }
}

#[derive(Debug, Clone, Default)]
pub struct TreeTraceResult {
    calls: Vec<FrameTraced>,
    call_idxs: Vec<usize>,
    error_call_idxs: Option<Vec<usize>>,
    error_message: Option<String>,
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

    fn pprint_child(
        calls: &[FrameTraced],
        tr: &mut ptree::TreeBuilder,
        label: &dyn Fn(&FrameTraced) -> String,
    ) {
        for child in calls.iter() {
            if child.subcalls.is_empty() {
                tr.add_empty_child(label(child));
            } else {
                tr.begin_child(label(child));
                Self::pprint_child(&child.subcalls, tr, label);
                tr.end_child();
            }
        }
    }

    fn pprint_call_tree_with(
        &self,
        root: &str,
        label: &dyn Fn(&FrameTraced) -> String,
    ) -> ptree::TreeBuilder {
        let mut tr = ptree::TreeBuilder::new(root.to_string());
        Self::pprint_child(&self.calls, &mut tr, label);
        tr
    }

    fn render_tree(&self, root: &str, label: &dyn Fn(&FrameTraced) -> String) -> String {
        let mut tr = self.pprint_call_tree_with(root, label);
        let mut buf = vec![];
        let out = std::io::Cursor::new(&mut buf);
        ptree::write_tree(&tr.build(), out).unwrap();
        String::from_utf8(buf).unwrap()
    }

    fn pprint_child_filtered(
        calls: &[FrameTraced],
        tr: &mut ptree::TreeBuilder,
        include: &dyn Fn(&FrameTraced) -> bool,
        label: &dyn Fn(&FrameTraced) -> String,
    ) {
        for child in calls.iter() {
            if include(child) {
                if child.subcalls.is_empty() {
                    tr.add_empty_child(label(child));
                } else {
                    tr.begin_child(label(child));
                    Self::pprint_child_filtered(&child.subcalls, tr, include, label);
                    tr.end_child();
                }
            } else {
                Self::pprint_child_filtered(&child.subcalls, tr, include, label);
            }
        }
    }

    fn render_tree_filtered(
        &self,
        root: &str,
        include: &dyn Fn(&FrameTraced) -> bool,
        label: &dyn Fn(&FrameTraced) -> String,
    ) -> String {
        let mut tr = ptree::TreeBuilder::new(root.to_string());
        Self::pprint_child_filtered(&self.calls, &mut tr, include, label);
        let mut buf = vec![];
        let out = std::io::Cursor::new(&mut buf);
        ptree::write_tree(&tr.build(), out).unwrap();
        String::from_utf8(buf).unwrap()
    }

    pub fn pprint(&self) -> String {
        self.render_tree("full_call_trace", &|frame| frame.full_label())
    }

    pub fn pprint_without_params(&self) -> String {
        self.render_tree_filtered(
            "full_call_trace_without_params",
            &|frame| !frame.is_framework_call(),
            &|frame| frame.name_only_label(),
        )
    }

    fn frame_at_path<'a>(&'a self, path: &[usize]) -> Option<&'a FrameTraced> {
        let (first, rest) = path.split_first()?;
        let mut current = self.calls.get(*first)?;
        for idx in rest {
            current = current.subcalls.get(*idx)?;
        }
        Some(current)
    }

    pub fn pprint_error_trace(&self) -> Option<String> {
        let path = self.error_call_idxs.as_ref()?;
        let mut out = String::from("error_call_trace:\n");
        for depth in 0..path.len() {
            let frame = self.frame_at_path(&path[..=depth])?;
            out.push_str(&"  ".repeat(depth));
            if depth > 0 {
                out.push_str("└─ ");
            }
            out.push_str(&frame.full_label());
            out.push('\n');
        }
        if let Some(msg) = &self.error_message {
            out.push_str(&"  ".repeat(path.len()));
            out.push_str("└─ ExecutionError: ");
            out.push_str(msg);
            out.push('\n');
        }
        Some(out)
    }

    pub fn into_raw(self) -> Vec<TraceEvent> {
        self.evs
    }

    pub fn pprint_failure_views(&self) -> String {
        let mut sections = vec![self.pprint()];
        if let Some(error_trace) = self.pprint_error_trace() {
            sections.push(error_trace);
        }
        sections.push(self.pprint_without_params());
        sections.join("\n")
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

    fn open_frame_inner(&mut self, frame: &Box<Frame>) {
        let inner = &mut self.inner;
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

    fn close_frame_inner(&mut self, return_: &Vec<TraceValue>) {
        let inner = &mut self.inner;
        let current = inner.current_frame();
        if current.is_none() {
            warn!("current frame is none when trying to close frame!?");
        } else {
            current.unwrap().close = Some(return_.clone());
        }
        inner.call_idxs.pop();
    }
}

impl MovySuiTracerExt for TreeTracer {
    fn on_raw_event(&mut self, _state: &TraceState, ev: &TraceEvent) -> bool {
        if let TraceEvent::Effect(effect) = ev
            && let Effect::ExecutionError(message) = effect.as_ref()
            && self.inner.error_call_idxs.is_none()
        {
            self.inner.error_call_idxs = Some(self.inner.call_idxs.clone());
            self.inner.error_message = Some(message.clone());
        }
        self.inner.evs.push(ev.clone());
        true
    }
    fn open_frame(&mut self, _state: &TraceState, frame: &Box<Frame>, _gas_left: u64) {
        self.open_frame_inner(frame);
    }
    fn close_frame(
        &mut self,
        _state: &TraceState,
        _frame_id: move_trace_format::format::TraceIndex,
        return_: &Vec<TraceValue>,
        _gas_left: u64,
    ) {
        self.close_frame_inner(return_);
    }
}
