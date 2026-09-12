use std::process::Command;

#[allow(dead_code)]
mod support;

#[test]
fn help_and_version_exit_successfully() {
    for argument in ["--help", "--version"] {
        let output = Command::new(env!("CARGO_BIN_EXE_agz-rust-mcp"))
            .arg(argument)
            .output()
            .expect("run packaged binary");
        assert!(
            output.status.success(),
            "{argument} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("agz-rust-mcp"),
            "{argument} omitted the binary name"
        );
    }
}

#[test]
fn bundled_skills_are_offline_and_export_without_overwriting() {
    use agz_rust_mcp::skills::BUNDLED_SKILLS;
    let root = support::TestRoot::new("skill-export");
    let binary = env!("CARGO_BIN_EXE_agz-rust-mcp");
    let listing = Command::new(binary)
        .args(["skills", "list"])
        .env("AGZ_RUST_MCP_UNKNOWN_TEST_SETTING", "invalid")
        .output()
        .expect("list skills without loading server config");
    assert!(listing.status.success(), "{:?}", listing);
    let names = String::from_utf8(listing.stdout).expect("UTF-8 names");
    for skill in BUNDLED_SKILLS {
        assert!(names.lines().any(|line| line.starts_with(skill.name)));
        let shown = Command::new(binary)
            .args(["skills", "show", skill.name])
            .output()
            .expect("show skill");
        assert!(shown.status.success());
        assert_eq!(shown.stdout, skill.markdown.as_bytes());
    }
    let destination = root.path().join("skills");
    let exported = Command::new(binary)
        .args(["skills", "export", "--dir"])
        .arg(&destination)
        .output()
        .expect("export skills");
    assert!(exported.status.success(), "{:?}", exported);
    for skill in BUNDLED_SKILLS {
        assert_eq!(
            std::fs::read(destination.join(skill.name).join("SKILL.md")).unwrap(),
            skill.markdown.as_bytes()
        );
    }
    let custom = destination.join(BUNDLED_SKILLS[0].name).join("SKILL.md");
    std::fs::write(&custom, "custom workflow").unwrap();
    let repeated = Command::new(binary)
        .args(["skills", "export", "--dir"])
        .arg(&destination)
        .output()
        .expect("repeat export");
    assert!(!repeated.status.success());
    assert_eq!(std::fs::read_to_string(custom).unwrap(), "custom workflow");
    assert!(
        !Command::new(binary)
            .args(["skills", "show", "missing"])
            .output()
            .unwrap()
            .status
            .success()
    );
}
