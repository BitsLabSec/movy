use move_binary_format::CompiledModule;

use crate::error::MovyError;

#[derive(Debug, Clone)]
pub enum MoveModule {
    // Sui
    Sui(CompiledModule),
    // Aptos
    // Aptos
}

impl MoveModule {
    pub fn from_sui_module_contents(contents: &[u8]) -> Result<Self, MovyError> {
        let module = CompiledModule::deserialize_with_defaults(contents)?;
        Ok(Self::Sui(module))
    }
    pub fn as_sui_module(&self) -> Option<&CompiledModule> {
        match self {
            Self::Sui(module) => Some(module),
        }
    }
}
