use move_trace_format::interface::Tracer;

pub mod concolic;
pub mod fuzz;
pub mod op;
pub mod oracle;
pub mod tree;

#[derive(Default)]
pub struct NopTracer;

impl Tracer for NopTracer {
    fn notify(
        &mut self,
        _event: &move_trace_format::format::TraceEvent,
        _writer: &mut move_trace_format::interface::Writer<'_>,
        _stack: Option<&move_vm_stack::Stack>,
    ) {
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
        writer: &mut move_trace_format::interface::Writer<'_>,
        stack: Option<&move_vm_stack::Stack>,
    ) {
        match self {
            Self::T1(t) => t.notify(event, writer, stack),
            Self::T2(t) => t.notify(event, writer, stack),
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
        writer: &mut move_trace_format::interface::Writer<'_>,
        stack: Option<&move_vm_stack::Stack>,
    ) {
        if let Some(tracer) = &mut self.tracer {
            tracer.notify(event, writer, stack);
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
        writer: &mut move_trace_format::interface::Writer<'_>,
        stack: Option<&move_vm_stack::Stack>,
    ) {
        self.t1.notify(event, writer, stack);
        self.t2.notify(event, writer, stack);
    }
}
