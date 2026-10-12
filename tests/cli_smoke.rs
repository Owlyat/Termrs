//! Black-box CLI tests for the `termrs` binary.
//!
//! These drive the built executable and assert on exit status + output, so
//! they need no access to crate internals. Unit tests live next to the code
//! they cover (`crates/termrs/src/test/*_tests.rs`); only windowless CLI paths
//! are exercised here (never `--selftest`, which opens a real window).

use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_termrs"))
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("spawn termrs binary")
}

#[test]
fn help_exits_successfully() {
    let out = run(&["--help"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("termrs"),
        "unexpected --help output: {stdout}"
    );
}

#[test]
fn version_reports_package_version() {
    let out = run(&["--version"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(env!("CARGO_PKG_VERSION")),
        "unexpected --version output: {stdout}"
    );
}

#[test]
fn print_default_config_outputs_toml() {
    let out = run(&["--print-default-config"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("[general]"),
        "default config should be TOML with a [general] section: {stdout}"
    );
}
