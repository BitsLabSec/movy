use move_trace_format::interface::Tracer;

pub mod concolic;
pub mod fuzz;
pub mod op;
pub mod oracle;
pub mod trace;
pub mod tree;
#[derive(Default)]
pub struct NopTracer;

impl Tracer for NopTracer {
    fn notify(
        &mut self,
        _event: &move_trace_format::format::TraceEvent,
        _writer: move_trace_format::interface::Writer<'_>,
    ) -> bool {
        true
    }
}

pub enum SelectiveTracer<T1, T2> {
    T1(T1),
    T2(T2),
}

impl<T1, T2> Tracer for SelectiveTracer<T1, T2>
where
    T1: Tracer,
    T2: Tracer,
{
    fn notify(
        &mut self,
        event: &move_trace_format::format::TraceEvent,
        writer: move_trace_format::interface::Writer<'_>,
    ) -> bool {
        match self {
            Self::T1(t) => t.notify(event, writer),
            Self::T2(t) => t.notify(event, writer),
        }
    }
}

#[derive(Default)]
pub struct MayEnableTracer<T> {
    pub tracer: Option<T>,
}

impl<T> MayEnableTracer<T> {
    pub fn new(tracer: T) -> Self {
        Self {
            tracer: Some(tracer),
        }
    }
}

impl<T> Tracer for MayEnableTracer<T>
where
    T: Tracer,
{
    fn notify(
        &mut self,
        event: &move_trace_format::format::TraceEvent,
        writer: move_trace_format::interface::Writer<'_>,
    ) -> bool {
        if let Some(tracer) = &mut self.tracer {
            tracer.notify(event, writer)
        } else {
            true
        }
    }
}

pub struct CombinedTracer<T1, T2> {
    pub t1: T1,
    pub t2: T2,
}

impl<T1, T2> Tracer for CombinedTracer<T1, T2>
where
    T1: Tracer,
    T2: Tracer,
{
    fn notify(
        &mut self,
        event: &move_trace_format::format::TraceEvent,
        writer: move_trace_format::interface::Writer<'_>,
    ) -> bool {
        self.t1.notify(event, writer)
    }
}
