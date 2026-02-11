use std::{collections::BTreeMap, marker::PhantomData};

use move_binary_format::CompiledModule;
use move_core_types::account_address::AccountAddress;
use move_trace_format::{
    format::TraceEvent,
    interface::{Tracer, Writer},
};
use movy_types::error::MovyError;

pub trait ModuleProvider {
    fn get_module(
        &mut self,
        address: AccountAddress,
        name: &str,
    ) -> Result<Option<CompiledModule>, MovyError>;
}

pub struct NoModuleProvider;

impl ModuleProvider for NoModuleProvider {
    fn get_module(
        &mut self,
        _address: AccountAddress,
        _name: &str,
    ) -> Result<Option<CompiledModule>, MovyError> {
        Ok(None)
    }
}

pub trait TraceNotifier {
    fn notify_event(&mut self, event: &TraceEvent) -> Result<(), MovyError>;
    fn handle_before_instruction(
        &mut self,
        _ctx: &TraceEvent,
        _extra: Option<&InstructionExtraInformation>,
    ) -> Result<(), MovyError> {
        Ok(())
    }
    fn notify(&mut self, event: &TraceEvent, writer: &mut Writer<'_>);
}

pub struct NotifierTracer<N, P = NoModuleProvider>
where
    N: TraceNotifier,
    P: ModuleProvider,
{
    pub notifier: N,
    module_provider: P,
    module_stack: Vec<(AccountAddress, String)>,
    module_cache: BTreeMap<(AccountAddress, String), CompiledModule>,
    _phantom: PhantomData<P>,
}

impl<N> NotifierTracer<N, NoModuleProvider>
where
    N: TraceNotifier,
{
    pub fn new(notifier: N) -> Self {
        Self {
            notifier,
            module_provider: NoModuleProvider,
            module_stack: Vec::new(),
            module_cache: BTreeMap::new(),
            _phantom: PhantomData,
        }
    }
}

impl<N, P> NotifierTracer<N, P>
where
    N: TraceNotifier,
    P: ModuleProvider,
{
    pub fn with_provider(notifier: N, provider: P) -> Self {
        Self {
            notifier,
            module_provider: provider,
            module_stack: Vec::new(),
            module_cache: BTreeMap::new(),
            _phantom: PhantomData,
        }
    }

    pub fn notifier_mut(&mut self) -> &mut N {
        &mut self.notifier
    }

    pub fn notifier(&self) -> &N {
        &self.notifier
    }

    pub fn into_inner(self) -> N {
        self.notifier
    }
    fn current_module(&self) -> Option<(AccountAddress, String)> {
        self.module_stack.last().cloned()
    }
    fn get_or_load_module(
        &mut self,
        address: AccountAddress,
        name: &str,
    ) -> Option<&CompiledModule> {
        let key = (address, name.to_string());

        if !self.module_cache.contains_key(&key) {
            if let Ok(Some(module)) = self.module_provider.get_module(address, name) {
                self.module_cache.insert(key.clone(), module);
            }
        }

        self.module_cache.get(&key)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum InstructionExtraInformation {
    Pack(usize),
    PackGeneric(usize),
    PackVariant(usize),
    PackVariantGeneric(usize),
    Unpack(usize),
    UnpackVariant(usize),
    UnpackGeneric(usize),
    UnpackVariantGeneric(usize),
}

fn create_instruction_extra<N, P>(
    tracer: &mut NotifierTracer<N, P>,
    event: &TraceEvent,
) -> Option<InstructionExtraInformation>
where
    N: TraceNotifier,
    P: ModuleProvider,
{
    match event {
        TraceEvent::Instruction { instruction, .. } => {
            use move_binary_format::file_format::Bytecode as B;
            use move_binary_format::file_format::StructFieldInformation;
            let mut extra = None;

            if let Some((address, name)) = tracer.current_module() {
                if let Some(module) = tracer.get_or_load_module(address, &name) {
                    match instruction {
                        B::Unpack(sidx) => {
                            let struct_def = module.struct_def_at(*sidx);
                            let field_count = match &struct_def.field_information {
                                StructFieldInformation::Native => 0,
                                StructFieldInformation::Declared(fields) => fields.len(),
                            };
                            extra = Some(InstructionExtraInformation::Unpack(field_count as usize));
                        }
                        B::UnpackVariant(vidx)
                        | B::UnpackVariantImmRef(vidx)
                        | B::UnpackVariantMutRef(vidx) => {
                            let variant_handle = module.variant_handle_at(*vidx);
                            let variant_def = module
                                .variant_def_at(variant_handle.enum_def, variant_handle.variant);
                            let field_count = variant_def.fields.len();
                            extra = Some(InstructionExtraInformation::UnpackVariant(
                                field_count as usize,
                            ));
                        }
                        B::UnpackGeneric(sidx) => {
                            let struct_inst = module.struct_instantiation_at(*sidx);
                            let struct_def = module.struct_def_at(struct_inst.def);
                            let field_count = match &struct_def.field_information {
                                StructFieldInformation::Native => 0,
                                StructFieldInformation::Declared(fields) => fields.len(),
                            };
                            extra = Some(InstructionExtraInformation::UnpackGeneric(
                                field_count as usize,
                            ));
                        }
                        B::UnpackVariantGeneric(vidx)
                        | B::UnpackVariantGenericImmRef(vidx)
                        | B::UnpackVariantGenericMutRef(vidx) => {
                            let variant_inst_handle = module.variant_instantiation_handle_at(*vidx);
                            let enum_inst =
                                module.enum_instantiation_at(variant_inst_handle.enum_def);
                            let variant_def =
                                module.variant_def_at(enum_inst.def, variant_inst_handle.variant);
                            let field_count = variant_def.fields.len();
                            extra = Some(InstructionExtraInformation::UnpackVariantGeneric(
                                field_count as usize,
                            ));
                        }
                        B::Pack(sidx) => {
                            let struct_def = module.struct_def_at(*sidx);
                            let field_count = match &struct_def.field_information {
                                StructFieldInformation::Native => 0,
                                StructFieldInformation::Declared(fields) => fields.len(),
                            };
                            extra = Some(InstructionExtraInformation::Pack(field_count as usize));
                        }
                        B::PackGeneric(sidx) => {
                            let struct_inst = module.struct_instantiation_at(*sidx);
                            let struct_def = module.struct_def_at(struct_inst.def);
                            let field_count = match &struct_def.field_information {
                                StructFieldInformation::Native => 0,
                                StructFieldInformation::Declared(fields) => fields.len(),
                            };
                            extra = Some(InstructionExtraInformation::PackGeneric(
                                field_count as usize,
                            ));
                        }
                        B::PackVariant(vidx) => {
                            let variant_handle = module.variant_handle_at(*vidx);
                            let variant_def = module
                                .variant_def_at(variant_handle.enum_def, variant_handle.variant);
                            let field_count = variant_def.fields.len();
                            extra = Some(InstructionExtraInformation::PackVariant(
                                field_count as usize,
                            ));
                        }
                        B::PackVariantGeneric(vidx) => {
                            let variant_inst_handle = module.variant_instantiation_handle_at(*vidx);
                            let enum_inst =
                                module.enum_instantiation_at(variant_inst_handle.enum_def);
                            let variant_def =
                                module.variant_def_at(enum_inst.def, variant_inst_handle.variant);
                            let field_count = variant_def.fields.len();
                            extra = Some(InstructionExtraInformation::PackVariantGeneric(
                                field_count as usize,
                            ));
                        }
                        _ => {}
                    }
                } else {
                }
            }

            extra
        }
        _ => None,
    }
}

impl<N, P> Tracer for NotifierTracer<N, P>
where
    N: TraceNotifier,
    P: ModuleProvider,
{
    fn notify(&mut self, event: &TraceEvent, mut writer: Writer<'_>) -> bool {
        self.notifier.notify(event, &mut writer);

        match event {
            TraceEvent::OpenFrame { frame, gas_left: _ } => {
                let address = *frame.module.address();
                let name = frame.module.name().to_string();
                // self.module_stack = Some((address, name));
                self.module_stack.push((address, name));
                if let Err(e) = self.notifier.notify_event(event) {
                    log::error!("NotifierTracer: failed to notify OpenFrame: {:?}", e);
                }
            }

            TraceEvent::CloseFrame {
                frame_id: _,
                return_: _,
                gas_left: _,
            } => {
                // self.module_stack = None;
                self.module_stack.pop();
                if let Err(e) = self.notifier.notify_event(event) {
                    log::error!("NotifierTracer: failed to notify CloseFrame: {:?}", e);
                }
            }

            TraceEvent::Instruction { .. } => {
                let extra = create_instruction_extra(self, event);
                if let Err(e) = self
                    .notifier
                    .handle_before_instruction(event, extra.as_ref())
                {
                    log::error!(
                        "NotifierTracer: failed to handle before instruction: {:?}",
                        e
                    );
                }
                if let Err(e) = self.notifier.notify_event(event) {
                    log::error!("NotifierTracer: failed to notify Instruction: {:?}", e);
                }
            }

            _ => {
                if let Err(e) = self.notifier.notify_event(event) {
                    log::error!("NotifierTracer: failed to notify event: {:?}", e);
                }
            }
        }
        true
    }
}
