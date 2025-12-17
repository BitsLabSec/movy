use std::fmt::Display;

use movy_types::input::{MoveAddress, MoveStructTag, MoveTypeTag, SuiObjectInputArgument};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct FlashWrapper {
    pub provider: FlashProvider,
    pub flash_coin: MoveStructTag,
    pub initial_flash_amount: u64,
}

impl Display for FlashWrapper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "Flash(source={}, coin={}, initial_amount={})",
            &self.provider, self.flash_coin, self.initial_flash_amount
        ))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub enum FlashProvider {
    Cetus {
        package: MoveAddress,
        coin_a: MoveTypeTag,
        coin_b: MoveTypeTag,
        global_config: SuiObjectInputArgument,
        pool: SuiObjectInputArgument,
        clock: SuiObjectInputArgument,
    },
    Nemo {
        package: MoveAddress,
        coin: MoveTypeTag,
        version: SuiObjectInputArgument,
        py_state: SuiObjectInputArgument,
        clock: SuiObjectInputArgument,
    },
}

impl Display for FlashProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cetus {
                package,
                coin_a,
                coin_b,
                global_config,
                pool,
                clock: _,
            } => f.write_fmt(format_args!(
                "Cetus(package={}, coins={}/{}, config={}, pool={})",
                package,
                coin_a,
                coin_b,
                global_config.id(),
                pool.id()
            )),
            Self::Nemo {
                package,
                coin,
                version,
                py_state,
                clock: _,
            } => f.write_fmt(format_args!(
                "Nemo(package={}, coin={}, version={}, py_state={})",
                package,
                coin,
                version.id(),
                py_state.id()
            )),
        }
    }
}
