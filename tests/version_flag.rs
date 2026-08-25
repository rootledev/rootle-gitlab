use std::process::{Command, Stdio};

#[test]
fn version_flag_smokes() {
    let out = Command::new(env!("CARGO_BIN_EXE_rootle-gitlab"))
        .arg("--version")
        .stdout(Stdio::piped())
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("rootle-gitlab "), "got: {text}");
}
