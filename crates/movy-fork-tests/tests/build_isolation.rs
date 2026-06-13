//! Guards for the two fork patches movy's per-spec build isolation relies on
//! (see `FORK_TESTS.md`, patches #14 and #15):
//!
//! * **#15 — extra source injection.** `move-package-alt-compilation`
//!   (`build_config.rs` + `compilation.rs::make_deps_for_compiler`) injects
//!   `BuildConfig.extra_source_files` into the ROOT package's target set, in
//!   test mode only. movy's audit harness uses this to compile a generated
//!   test into the audited package's named-address scope without copying it
//!   into the package on disk.
//! * **#14 — output-keyed build lock + artifact redirection.**
//!   `move-package-alt` (`root_package.rs`) keys the per-package build lock on
//!   `output_path` rather than `input_path`, and the package system writes all
//!   build artifacts under `install_dir` (the output dir) instead of next to
//!   the sources. movy relies on this to run many concurrent `movy sui test`
//!   processes over the SAME read-only source tree with distinct install dirs.
//!
//! These tests don't need the offline executor harness — they exercise the
//! compile path directly via `SuiCompiledPackage::build`.

use std::io::Write as _;

use movy_sui::compile::{BuildIsolation, SuiCompiledPackage};

/// Write a minimal package whose `main` module *depends on* a module that only
/// exists in an externally-supplied extra source file. Returns `(tempdir,
/// extra_source_path)`. The package itself is incomplete on disk: it only
/// compiles if the extra file is injected into its compile.
fn package_needing_injected_extra() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let toml = "[package]\nname = \"p\"\nedition = \"2024.beta\"\n\n\
                [dependencies]\n\n[addresses]\np = \"0x0\"\n\n\
                [dev-dependencies]\n\n[dev-addresses]\n";
    std::fs::write(dir.path().join("Move.toml"), toml).unwrap();
    std::fs::create_dir_all(dir.path().join("sources")).unwrap();
    // main (on disk) references `p::injected`, which is NOT in sources/.
    std::fs::write(
        dir.path().join("sources/main.move"),
        "#[test_only]\nmodule p::main;\nuse p::injected;\n\
         #[test] fun t() { assert!(injected::marker() == 42, 0); }\n",
    )
    .unwrap();
    // The injected module lives OUTSIDE sources/, fed only via extra_sources.
    let extra = dir.path().join("injected.move");
    let mut fp = std::fs::File::create(&extra).unwrap();
    fp.write_all(b"#[test_only]\nmodule p::injected;\npublic fun marker(): u64 { 42 }\n")
        .unwrap();
    (dir, extra)
}

/// #15: `extra_source_files` are compiled into the root package's test-mode
/// build. The package's `main` only resolves `use p::injected` when the extra
/// file is injected — so a successful build *is* the proof of injection.
///
/// bite: the control build below (no extra sources) must FAIL to resolve
/// `p::injected`; if injection silently stopped happening, the first assert
/// fails instead.
#[test]
fn extra_sources_inject_into_root_test_build() {
    let (dir, extra) = package_needing_injected_extra();

    let with_extra = BuildIsolation {
        install_dir: None,
        extra_sources: vec![extra],
    };
    let ok = SuiCompiledPackage::build(
        dir.path(),
        /* test_mode */ true,
        /* with_unpublished */ true,
        &with_extra,
    );
    assert!(
        ok.is_ok(),
        "extra_sources must be injected into the root test build so `use p::injected` resolves: {:?}",
        ok.err()
    );

    // Control / bite: without the extra file, `p::injected` is undefined.
    let without = SuiCompiledPackage::build(dir.path(), true, true, &BuildIsolation::default());
    assert!(
        without.is_err(),
        "control: without extra_sources the same package must fail (proves the success above came from injection, not from the file being on disk)"
    );
}

/// #14: build artifacts go under `install_dir` (the output dir), leaving the
/// source tree untouched. This is the property that lets many concurrent
/// builds share one read-only source tree — each writes to its own install
/// dir, and the per-package lock is keyed on that output dir.
///
/// bite: if `install_dir` redirection regressed (artifacts written next to the
/// sources), `<source>/build` would appear and this fails.
#[test]
fn install_dir_redirects_artifacts_off_source_tree() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let toml = "[package]\nname = \"q\"\nedition = \"2024.beta\"\n\n\
                [dependencies]\n\n[addresses]\nq = \"0x0\"\n\n\
                [dev-dependencies]\n\n[dev-addresses]\n";
    std::fs::write(dir.path().join("Move.toml"), toml).unwrap();
    std::fs::create_dir_all(dir.path().join("sources")).unwrap();
    std::fs::write(
        dir.path().join("sources/q.move"),
        "module q::q;\npublic fun f(): u64 { 7 }\n",
    )
    .unwrap();

    let install = tempfile::TempDir::new().expect("install tempdir");
    let iso = BuildIsolation {
        install_dir: Some(install.path().to_path_buf()),
        extra_sources: vec![],
    };
    let res = SuiCompiledPackage::build(dir.path(), false, true, &iso);
    assert!(res.is_ok(), "build failed: {:?}", res.err());

    assert!(
        !dir.path().join("build").exists(),
        "source tree must stay clean: artifacts belong under install_dir, not <source>/build"
    );
    assert!(
        install.path().join("build").exists(),
        "install_dir must receive the build artifacts"
    );
}
