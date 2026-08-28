// run.sh is a Unix shell script that execs cargo — it cannot run on Windows.
// The script is a convenience for Linux/WSL development only.
#![cfg(not(windows))]

use std::process::Command;

#[test]
fn run_sh_without_args_does_not_open_repo_tmp_tsv() {
    let out = Command::new("./run.sh").arg("--version").output().expect("failed to run");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("corro"));
}
