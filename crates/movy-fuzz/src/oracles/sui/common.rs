use move_core_types::language_storage::ModuleId;
use move_core_types::{account_address::AccountAddress, identifier::Identifier};
use movy_types::input::FunctionIdent;

pub fn format_vulnerability_info(
    base: &str,
    current_function: Option<&(ModuleId, String)>,
    pc: Option<u16>,
) -> String {
    let mut parts = vec![base.to_string()];
    if let Some((module_id, function)) = current_function {
        parts.push(format!(
            "location={}::{}::{}",
            module_id.address().to_canonical_string(true),
            module_id.name(),
            function
        ));
    }
    if let Some(pc) = pc {
        parts.push(format!("pc={}", pc));
    }
    parts.join(" | ")
}

pub fn to_module_func(fid: &FunctionIdent) -> Option<(ModuleId, String)> {
    let addr: AccountAddress = fid.0.module_address.into();
    let Ok(name) = Identifier::new(fid.0.module_name.clone()) else {
        return None;
    };
    Some((ModuleId::new(addr, name), fid.1.clone()))
}
