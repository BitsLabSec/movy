use movy_sui::compile::{SuiCompiledPackage, build_package_resolved};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

macro_rules! cargo_print {
    ($($tokens: tt)*) => {
        println!("cargo:warning={}", format!($($tokens)*))
    }
}

fn build_std(dir: &Path, test: bool) -> Vec<SuiCompiledPackage> {
    let previous_build = dir.join("build");
    if previous_build.exists() {
        std::fs::remove_dir_all(&previous_build).unwrap();
    }
    let (_, resolved) = build_package_resolved(dir, test).unwrap();
    let flag = if test { "testing" } else { "non-testing" };
    let mut deps = vec![];
    for (package_name, package) in resolved.package_table.iter() {
        if package_name.as_str() != "hello_std" {
            cargo_print!(
                "Building {} std {} at {}",
                flag,
                package_name.as_str(),
                package.package_path.display()
            );

            let out =
                SuiCompiledPackage::build_all_unpublished_from_folder(&package.package_path, test)
                    .unwrap();
            let build_directory = package.package_path.join("build");
            if build_directory.exists() {
                std::fs::remove_dir_all(&build_directory).unwrap();
            }
            deps.push(out);
        }
    }
    deps
}

fn main() {
    println!("cargo::rerun-if-env-changed=STD_BUILD_KEEP");
    let std_toml = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hello_std/Move.toml"));
    let std_lock = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/hello_std/Move.lock"));
    let mut dir = tempfile::TempDir::new().unwrap();
    if std::env::var("STD_BUILD_KEEP").is_ok() {
        dir.disable_cleanup(true);
    }
    let mut fp = std::fs::File::create(dir.path().join("Move.toml")).unwrap();
    fp.write_all(std_toml.as_bytes()).unwrap();
    let mut fp = std::fs::File::create(dir.path().join("Move.lock")).unwrap();
    fp.write_all(std_lock.as_bytes()).unwrap();
    fp.flush().unwrap();
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let testing_stds = build_std(dir.path(), true);
    let non_testing_std = build_std(dir.path(), false);

    let fp = std::fs::File::create(out_dir.join("std.testing")).unwrap();
    serde_json::to_writer(fp, &testing_stds).unwrap();

    let fp = std::fs::File::create(out_dir.join("std")).unwrap();
    serde_json::to_writer(fp, &non_testing_std).unwrap();
}
