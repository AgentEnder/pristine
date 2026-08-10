//! The whole of the scaffold's behaviour: `pristine --version` reports the crate version.
//!
//! The binary is `pristine` while the package is `pristine-cli`, so this also pins the
//! `[[bin]]` rename in place — `CARGO_BIN_EXE_pristine` only resolves if the binary kept
//! the plain name.

use std::process::Command;

#[test]
fn version_flag_reports_the_crate_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_pristine"))
        .arg("--version")
        .output()
        .expect("the pristine binary should be runnable");

    assert!(
        output.status.success(),
        "`pristine --version` exited with {:?}",
        output.status.code()
    );

    let stdout = String::from_utf8(output.stdout).expect("--version output should be utf-8");
    assert_eq!(
        stdout.trim(),
        format!("pristine {}", env!("CARGO_PKG_VERSION")),
        "--version should name the binary, not the package"
    );
}
