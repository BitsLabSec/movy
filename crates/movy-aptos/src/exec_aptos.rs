use aptos_crypto::{PrivateKey, SigningKey, Uniform, ed25519::Ed25519PrivateKey};
use aptos_move_core_types::account_address::AccountAddress;
use aptos_move_core_types::vm_status::VMStatus;
use aptos_types::{
    chain_id::ChainId,
    transaction::{
        AuxiliaryInfo, PersistedAuxiliaryInfo, RawTransaction, SignedTransaction, TransactionPayload,
    },
};
use aptos_vm::AptosVM;
use aptos_vm::gas::make_prod_gas_meter;
use aptos_vm_logging::log_schema::AdapterLogSchema;
use aptos_move_vm_runtime::execution_tracing::TraceRecorder;
use std::{ops::Deref, time::SystemTime};
use movy_types::error::MovyError;

use super::aptos_custom_state::AptosCustomState;
use super::custom_state_view::CustomStateView;

/// Core Aptos transaction executor with minimal dependencies.
/// Executes transactions without fuzzing-related instrumentation.
pub struct AptosMoveExecutor {
    aptos_vm: AptosVM,
}

/// Execution results containing VM status and execution details
pub struct ExecutionResults {
    pub vm_status: VMStatus,
    // TODO: Add more execution details like gas usage, events, etc.
}

/// Execution results with optional tracer
pub struct ExecutionTracedResults<R> {
    pub results: ExecutionResults,
    pub tracer: Option<R>,
}

impl<R> Deref for ExecutionTracedResults<R> {
    type Target = ExecutionResults;
    fn deref(&self) -> &Self::Target {
        &self.results
    }
}

/// Creates a signed transaction from a payload.
fn create_signed_transaction(
    payload: TransactionPayload,
    sequence_number: u64,
    max_gas_amount: u64,
    gas_unit_price: u64,
) -> SignedTransaction {
    let sender = AccountAddress::ZERO;
    let expiration_timestamp_secs = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 60;

    let raw_transaction = RawTransaction::new(
        sender,
        sequence_number,
        payload,
        max_gas_amount,
        gas_unit_price,
        expiration_timestamp_secs,
        ChainId::test(),
    );
    let private_key_1 = Ed25519PrivateKey::generate_for_testing();
    let signature = private_key_1.sign(&raw_transaction).unwrap();

    SignedTransaction::new(
        raw_transaction.clone(),
        private_key_1.public_key(),
        signature.clone(),
    )
}

impl AptosMoveExecutor {
    /// Creates a new executor instance.
    pub fn new() -> Self {
        AptosVM::set_concurrency_level_once(1);
        let env = super::aptos_custom_state::AptosCustomState::default_env();
        Self {
            aptos_vm: AptosVM::new(&env),
        }
    }

    /// Executes a single transaction on the given state.
    ///
    /// # Arguments
    /// * `transaction` - The transaction payload to execute
    /// * `state` - The Aptos state for execution
    /// * `tracer` - Custom execution tracer (implements TraceRecorder trait)
    ///
    /// Returns a Result containing:
    /// - Ok(ExecutionTracedResults<R>): execution results with tracer
    /// - Err(MovyError): execution failed with error details
    ///
    /// This is the core transaction execution without any fuzzing instrumentation.
    pub fn execute_transaction<R: TraceRecorder>(
        &mut self,
        transaction: TransactionPayload,
        state: &AptosCustomState,
        tracer: R,
    ) -> Result<ExecutionTracedResults<R>, MovyError> {
        match &transaction {
            TransactionPayload::EntryFunction(_) | TransactionPayload::Script(_) => {
                let view = CustomStateView::new(state);
                let code_storage =
                    aptos_vm_types::module_and_script_storage::AsAptosCodeStorage::as_aptos_code_storage(&view, state);

                let env = super::aptos_custom_state::AptosCustomState::default();
                let log_context = AdapterLogSchema::new(env.id(), 0);
                
                let result = self.aptos_vm.execute_user_payload_no_checking_with_tracer(
                    state,
                    &code_storage,
                    &create_signed_transaction(transaction, 0, 1_000_000, 1),
                    &log_context,
                    make_prod_gas_meter,
                    &AuxiliaryInfo::new(
                        PersistedAuxiliaryInfo::V1 {
                            transaction_index: 0,
                        },
                        None,
                    ),
                    tracer,
                );
                
                match result {
                    Ok((vmstatus, _output, _pcs, _shifts, _)) => {
                        Ok(ExecutionTracedResults {
                            results: ExecutionResults {
                                vm_status: vmstatus,
                            },
                            tracer: None, // tracer is consumed by the VM execution
                        })
                    }
                    Err(vmstatus) => Err(MovyError::Other(
                        color_eyre::eyre::eyre!("VM execution failed: {:?}", vmstatus)
                    )),
                }
            }
            _ => {
                Err(MovyError::Unsupported("Unsupported payload type for this executor".to_string()))
            }
        }
    }
}

impl Default for AptosMoveExecutor {
    fn default() -> Self {
        Self::new()
    }
}
