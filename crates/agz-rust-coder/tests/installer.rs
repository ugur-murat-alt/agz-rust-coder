#[cfg(target_os = "linux")]
#[test]
fn installer_handles_paths_and_rejects_invalid_artifacts() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/installer.sh");
    let output = std::process::Command::new("bash")
        .arg(script)
        .output()
        .expect("run hermetic installer tests");
    assert!(
        output.status.success(),
        "installer regressions failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
