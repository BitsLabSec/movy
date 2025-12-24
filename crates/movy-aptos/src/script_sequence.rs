use std::collections::HashMap;

use aptos_dynamic_transaction_composer::{ArgumentOperation, CallArgument, PreviousResult, TransactionComposer};
use aptos_move_core_types::account_address::AccountAddress;
use aptos_move_core_types::identifier::Identifier;
use aptos_move_core_types::language_storage::{ModuleId, TypeTag};
use aptos_move_core_types::int256::U256;
use aptos_types::transaction::{Script, TransactionArgument};
use bcs;
use bytes::Bytes;
use log::warn;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScriptSequence {
    calls: Vec<SequenceCall>,
}

impl ScriptSequence {
    pub fn new() -> Self {
        Self { calls: Vec::new() }
    }

    pub fn calls(&self) -> &[SequenceCall] {
        &self.calls
    }

    pub fn push_call(&mut self, call: SequenceCall) {
        self.calls.push(call);
    }

    pub fn len(&self) -> usize {
        self.calls.len()
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

#[derive(Serialize, Deserialize)]
struct PreviousResultRepr {
    call_idx: u16,
    return_idx: u16,
    operation_type: ArgumentOperation,
}

fn build_previous_result(call_idx: u16, return_idx: u16) -> Option<PreviousResult> {
    let repr = PreviousResultRepr {
        call_idx,
        return_idx,
        operation_type: ArgumentOperation::Move,
    };
    let bytes = bcs::to_bytes(&repr).ok()?;
    bcs::from_bytes(&bytes).ok()
}

fn collect_txn_args(sequence: &ScriptSequence) -> Option<Vec<TransactionArgument>> {
    let mut args = Vec::new();
    for call in sequence.calls() {
        for arg in call.args() {
            match arg {
                SequenceArgument::Raw { bytes, ty } => {
                    args.push(bytes_to_transaction_argument(ty, bytes)?);
                }
                SequenceArgument::Signer(_) => {
                    // Signers are handled by the VM; no serialized argument
                    // needed.
                }
                SequenceArgument::PreviousResult { .. } => {
                    // Previous results don't introduce new serialized
                    // parameters.
                }
            }
        }
    }
    Some(args)
}

fn bytes_to_transaction_argument(ty: &TypeTag, bytes: &[u8]) -> Option<TransactionArgument> {
    macro_rules! decode {
        ($t:ty) => {
            bcs::from_bytes::<$t>(bytes).ok()
        };
    }
    match ty {
        TypeTag::Bool => decode!(bool).map(TransactionArgument::Bool),
        TypeTag::U8 => decode!(u8).map(TransactionArgument::U8),
        TypeTag::U16 => decode!(u16).map(TransactionArgument::U16),
        TypeTag::U32 => decode!(u32).map(TransactionArgument::U32),
        TypeTag::U64 => decode!(u64).map(TransactionArgument::U64),
        TypeTag::U128 => decode!(u128).map(TransactionArgument::U128),
        TypeTag::U256 => decode!(U256).map(TransactionArgument::U256),
        TypeTag::Address => decode!(AccountAddress).map(TransactionArgument::Address),
        TypeTag::Vector(inner) => match inner.as_ref() {
            TypeTag::U8 => decode!(Vec<u8>).map(TransactionArgument::U8Vector),
            _ => Some(TransactionArgument::Serialized(bytes.to_vec())),
        },
        _ => Some(TransactionArgument::Serialized(bytes.to_vec())),
    }
}

#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct SequenceCall {
    module: ModuleId,
    function: Identifier,
    ty_args: Vec<TypeTag>,
    args: Vec<SequenceArgument>,
}

impl SequenceCall {
    pub fn new(module: ModuleId, function: Identifier, ty_args: Vec<TypeTag>, args: Vec<SequenceArgument>) -> Self {
        Self {
            module,
            function,
            ty_args,
            args,
        }
    }

    pub fn module(&self) -> &ModuleId {
        &self.module
    }

    pub fn function(&self) -> &Identifier {
        &self.function
    }

    pub fn ty_args(&self) -> &[TypeTag] {
        &self.ty_args
    }

    pub fn args(&self) -> &[SequenceArgument] {
        &self.args
    }
}

#[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum SequenceArgument {
    Signer(u16),
    Raw { bytes: Vec<u8>, ty: TypeTag },
    PreviousResult { call_idx: u16, return_idx: u16 },
}

pub fn compile_sequence(sequence: &ScriptSequence, modules: &HashMap<ModuleId, Bytes>) -> Option<Script> {
    let mut composer = TransactionComposer::multi_signer(0);
    for (module_id, bytes) in modules {
        if let Err(err) = composer.store_module(bytes.to_vec()) {
            warn!(
                "[aptos-fuzzer] failed to load module {} into composer: {}",
                module_id, err
            );
            return None;
        }
    }

    for call in sequence.calls() {
        let args = call
            .args()
            .iter()
            .map(|arg| match arg {
                SequenceArgument::Signer(idx) => Some(CallArgument::new_signer(*idx)),
                SequenceArgument::Raw { bytes, .. } => Some(CallArgument::new_bytes(bytes.clone())),
                SequenceArgument::PreviousResult { call_idx, return_idx } => {
                    build_previous_result(*call_idx, *return_idx).map(CallArgument::PreviousResult)
                }
            })
            .collect::<Option<Vec<_>>>();
        let Some(args) = args else {
            warn!(
                "[aptos-fuzzer] failed to rebuild arguments for {}::{}",
                call.module(),
                call.function()
            );
            return None;
        };

        let ty_args: Vec<String> = call.ty_args().iter().map(|tag| tag.to_canonical_string()).collect();

        let module_str = call.module().short_str_lossless();
        if let Err(err) = composer.add_batched_call(module_str, call.function().to_string(), ty_args, args) {
            warn!(
                "[aptos-fuzzer] failed to add batched call {}::{}: {}",
                call.module(),
                call.function(),
                err
            );
            return None;
        }
    }

    let txn_args = match collect_txn_args(sequence) {
        Some(args) => args,
        None => return None,
    };

    match composer.generate_batched_calls(true) {
        Ok(bytes) => match bcs::from_bytes::<Script>(&bytes) {
            Ok(script) => {
                let (code, ty_args, _old_args) = script.into_inner();
                Some(Script::new(code, ty_args, txn_args))
            }
            Err(err) => {
                warn!("[aptos-fuzzer] failed to decode generated script payload: {}", err);
                None
            }
        },
        Err(err) => {
            warn!(
                "[aptos-fuzzer] failed to serialize script sequence into script: {}",
                err
            );
            None
        }
    }
}
