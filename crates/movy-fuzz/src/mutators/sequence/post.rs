use std::collections::BTreeMap;

use movy_types::{
    abi::MoveAbiSignatureToken,
    input::{
        InputArgument, MoveAddress, MoveCall, MoveSequence, MoveSequenceCall, SequenceArgument,
    },
};

use crate::{meta::HasFuzzMetadata, mutators::object_data::ObjectData, state::HasFuzzEnv};

pub fn process_key_store(ptb: &mut MoveSequence, state: &(impl HasFuzzMetadata + HasFuzzEnv)) {
    let object_data = ObjectData::from_ptb(ptb, state);
    if !object_data.key_store_objects.is_empty() {
        ptb.inputs
            .push(InputArgument::Address(state.fuzz_state().attacker));
        let to_object_cmd = MoveSequenceCall::TransferObjects(
            object_data.key_store_objects,
            SequenceArgument::Input(ptb.inputs.len() as u16 - 1),
        );
        ptb.commands.push(to_object_cmd);
    }
}

pub fn remove_process_key_store(ptb: &mut MoveSequence) {
    if let Some(MoveSequenceCall::TransferObjects(_, SequenceArgument::Input(_))) =
        ptb.commands.last()
    {
        ptb.commands.pop();
        ptb.inputs.pop();
    }
}

pub fn process_balance(ptb: &mut MoveSequence, state: &(impl HasFuzzMetadata + HasFuzzEnv)) {
    let object_data = ObjectData::from_ptb(ptb, state);
    for balance in object_data.balances.iter() {
        let (idx, ret_idx) = if let SequenceArgument::Result(i) = balance {
            (*i, 0)
        } else if let SequenceArgument::NestedResult(i, j) = balance {
            (*i, *j)
        } else {
            panic!("Expected balance argument to be Result or NestedResult");
        };
        let MoveSequenceCall::Call(movecall) = &mut ptb.commands[idx as usize] else {
            panic!("Expected MoveCall command");
        };
        let function = state
            .fuzz_state()
            .get_function(
                &movecall.module_id,
                &movecall.module_name,
                &movecall.function,
            )
            .unwrap();
        let Some(MoveAbiSignatureToken::StructInstantiation(_, type_arguments)) =
            function.return_paramters.get(ret_idx as usize)
        else {
            panic!("Expected balance return type to be a struct");
        };
        let ty_arg = if let MoveAbiSignatureToken::TypeParameter(j, _) = type_arguments[0] {
            let ty_arg = &movecall.type_arguments[j as usize];
            ty_arg.clone()
        } else {
            type_arguments[0].subst(&BTreeMap::new()).unwrap()
        };
        let from_balance_cmd = MoveSequenceCall::Call(MoveCall {
            module_id: MoveAddress::two(),
            module_name: "coin".to_string(),
            function: "from_balance".to_string(),
            type_arguments: vec![ty_arg.clone()],
            arguments: vec![*balance],
        });
        ptb.commands.push(from_balance_cmd);
    }
}

pub fn remove_process_balance(ptb: &mut MoveSequence) {
    for i in (0..ptb.commands.len()).rev() {
        if let MoveSequenceCall::Call(movecall) = &ptb.commands[i] {
            if movecall.module_id == MoveAddress::two()
                && movecall.module_name == "coin"
                && movecall.function == "from_balance"
            {
                ptb.commands.remove(i);
            } else {
                break;
            }
        } else {
            break;
        }
    }
}
