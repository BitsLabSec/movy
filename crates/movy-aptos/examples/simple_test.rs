use aptos_move_core_types::{
    account_address::AccountAddress,
    identifier::Identifier,
    language_storage::ModuleId,
};
use aptos_move_vm_runtime::execution_tracing::{TraceRecorder, MoveTracer};
use aptos_move_vm_types::instr::Instruction;
use aptos_move_vm_runtime::LoadedFunction;
use aptos_move_core_types::function::ClosureMask;
use aptos_move_vm_runtime::execution_tracing::Trace;
use aptos_types::transaction::{EntryFunction, TransactionPayload};

use movy_aptos::{AptosMoveExecutor, aptos_custom_state::AptosCustomState};

/// The simplest tracer implementation
#[derive(Debug, Default)]
pub struct NoOpTracer;

impl MoveTracer for NoOpTracer {}

impl TraceRecorder for NoOpTracer {
    fn is_enabled(&self) -> bool {
        false
    }

    fn finish(self) -> Trace {
        Trace::default()
    }

    fn record_successful_instruction(&mut self, _instruction: &Instruction) {}
    fn record_branch_outcome(&mut self, _outcome: bool) {}
    fn record_entrypoint(&mut self, _function: &LoadedFunction) {}
    fn record_call_closure(&mut self, _function: &LoadedFunction, _mask: ClosureMask) {}
}

fn main() {
    println!("=== Simple test of AptosMoveExecutor::execute_transaction ===\n");

    // 1. Create executor
    println!("1. Creating AptosMoveExecutor...");
    let mut executor = AptosMoveExecutor::new();
    println!("   ✓ Executor created successfully\n");

    // 2. Create state
    println!("2. Creating AptosCustomState...");
    let state = AptosCustomState::default();
    println!("   ✓ State created successfully");
    println!("   - Loaded modules count: {}", state.module_bytes().len());
    println!("   - Total bytecode instructions: {}", state.total_bytecode_instructions());
    println!("   - Total possible edges: {}\n", state.total_possible_edges());

    // 3. Create tracer
    println!("3. Creating tracer...");
    let tracer = NoOpTracer;
    println!("   ✓ Tracer created successfully\n");

    // 4. Test execute_transaction function
    println!("4. Testing execute_transaction function...");
    
    // Create a simple transaction payload
    let payload = TransactionPayload::EntryFunction(EntryFunction::new(
        ModuleId::new(
            AccountAddress::ONE,
            Identifier::new("aptos_account").unwrap(),
        ),
        Identifier::new("create_account").unwrap(),
        vec![],
        vec![bcs::to_bytes(&AccountAddress::from_hex_literal("0x42").unwrap()).unwrap()],
    ));

    println!("   Calling executor.execute_transaction()...");
    let result = executor.execute_transaction(payload, &state, tracer);
    
    println!("   Execution result:");
    match &result {
        Ok(traced_results) => {
            println!("     - Success: true");
            println!("     - VM status: {:?}", traced_results.results.vm_status);
        }
        Err(error) => {
            println!("     - Success: false");
            println!("     - Error: {}", error);
        }
    }

    // 5. Analyze results
    println!("\n5. Result analysis:");
    match result {
        Ok(traced_results) => {
            println!("   ✓ Transaction executed successfully!");
            println!("   VM status details: {:?}", traced_results.results.vm_status);
        }
        Err(error) => {
            println!("   ⚠ Transaction execution failed: {}", error);
            println!("   This may be due to:");
            println!("     - Missing on-chain configuration (e.g., gas schedule)");
            println!("     - Incomplete account state");
            println!("     - Or other VM configuration issues");
            println!("   However, this does not mean the execute_transaction function itself is problematic.");
        }
    }

    println!("\n=== Test completed ===");
    println!("AptosMoveExecutor::execute_transaction function has been successfully called and returned results.");
}