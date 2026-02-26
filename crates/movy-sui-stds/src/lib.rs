use std::collections::BTreeMap;

use movy_sui::compile::SuiCompiledPackage;
use movy_types::{abi::MovePackageAbi, input::MoveAddress};

pub fn testing_std() -> Vec<SuiCompiledPackage> {
    let bs = include_bytes!(concat!(env!("OUT_DIR"), "/std.testing"));
    bcs::from_bytes(bs).unwrap()
}

pub fn sui_std() -> Vec<SuiCompiledPackage> {
    let bs = include_bytes!(concat!(env!("OUT_DIR"), "/std"));
    bcs::from_bytes(bs).unwrap()
}

pub fn std_abi(test: bool) -> BTreeMap<MoveAddress, MovePackageAbi> {
    let stds = if test { testing_std() } else { sui_std() };
    let mut out = BTreeMap::new();
    for std in stds {
        out.insert(std.package_id.into(), std.abi().unwrap());
    }
    out
}

pub fn movy() -> SuiCompiledPackage {
    let bs = include_bytes!(concat!(env!("OUT_DIR"), "/movy"));
    bcs::from_bytes(bs).unwrap()
}
