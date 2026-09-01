use std::process::Command;

#[test]
fn help_and_version_exit_successfully() {
    for argument in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_agz-rust-coder"))
            .arg(argument)
            .output()
            .expect("run packaged binary");
        assert!(
            output.status.success(),
            "{argument} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("agz-rust-coder"),
            "{argument} omitted the binary name"
        );
    }
}
