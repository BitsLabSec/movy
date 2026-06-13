use movy_sui::compile::SuiCompiledPackage;
use serde::Serialize;
use std::{
    io::Write,
    path::{Path, PathBuf},
};

macro_rules! cargo_print {
    ($($tokens: tt)*) => {
        println!("cargo:warning={}", format!($($tokens)*))
    }
}

fn clear_build(dir: &Path) {
    let previous_build = dir.join("build");
    if previous_build.exists() {
        std::fs::remove_dir_all(&previous_build).unwrap();
    }
}

fn build_movy(dir: &Path) -> SuiCompiledPackage {
    clear_build(dir);
    SuiCompiledPackage::build_all_unpublished_from_folder(dir, false, &movy_sui::compile::BuildIsolation::default()).unwrap()
}

fn build_std(dir: &Path, test: bool) -> Vec<SuiCompiledPackage> {
    let _flag = if test { "testing" } else { "non-testing" };
    let mut out = vec![];
    for package in [
        "bridge",
        "deepbook",
        "move-stdlib",
        "sui-framework",
        "sui-system",
    ] {
        let package = dir.join(package);
        clear_build(&package);
        out.push(SuiCompiledPackage::build_checked(&package, test, false, true, &movy_sui::compile::BuildIsolation::default()).unwrap());
    }
    out
}

fn write_bcs<T: Serialize>(path: &Path, val: T) {
    let mut fp = std::fs::File::create(path).unwrap();
    let bytes = bcs::to_bytes(&val).unwrap();
    fp.write_all(&bytes).unwrap();
}

fn main() {
    let std = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/sui-framework/packages"
    ));
    let movy = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/movy"));
    println!("cargo::rerun-if-changed={}", movy.join("sources").display());
    println!(
        "cargo::rerun-if-changed={}",
        movy.join("Move.toml").display()
    );

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let movy = build_movy(&movy);
    let testing_stds = build_std(&std, true);
    let non_testing_std = build_std(&std, false);

    write_bcs(&out_dir.join("std.testing"), &testing_stds);
    write_bcs(&out_dir.join("std"), &non_testing_std);
    write_bcs(&out_dir.join("movy"), &movy);
}
