//! Read-only measurement harness, compatible with the 0.1.1 baseline API.
use agz_rust_coder::workspace::{
    ClientRoots, GitOutput, GitProbe, IdentityError, IdentityInput, RootGuard,
    compute_input_identity,
};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    time::Instant,
};
struct NoGit;
impl GitProbe for NoGit {
    fn run(&self, _: &Path, _: &[OsString], _: usize) -> Result<GitOutput, IdentityError> {
        Ok(GitOutput {
            status: Some(128),
            stdout: Vec::new(),
            truncated: false,
        })
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    if args.len() != 2 {
        return Err("usage: identity_measure ABSOLUTE_FIXTURE SAMPLE_COUNT".into());
    }
    let path = std::fs::canonicalize(PathBuf::from(&args[0]))?;
    let samples = args[1]
        .to_str()
        .ok_or("non-UTF-8 sample count")?
        .parse::<usize>()?;
    if !(5..=100).contains(&samples) {
        return Err("sample count must be 5..=100".into());
    }
    let guard = RootGuard::new([path.clone()], std::iter::empty())?;
    let roots = guard.snapshot(ClientRoots::unsupported())?;
    let root = roots.select(None)?;
    let manifest = path.join("Cargo.toml");
    // No Cargo command is launched: fixed, identical inputs for both binaries.
    let cargo = path.join("cargo-placeholder");
    let command = vec![OsString::from("check")];
    let environment = BTreeMap::from([
        (OsString::from("HOME"), path.as_os_str().to_owned()),
        (
            OsString::from("CARGO_HOME"),
            path.join("cargo-home").into_os_string(),
        ),
    ]);
    let input = IdentityInput::new(&root, &manifest, &cargo, &command, &environment, &NoGit);
    for _ in 0..3 {
        let _ = compute_input_identity(&input)?;
    }
    let mut times = Vec::new();
    let mut hashes = Vec::new();
    let mut files = 0;
    for _ in 0..samples {
        let start = Instant::now();
        let identity = compute_input_identity(&input)?;
        times.push(start.elapsed().as_secs_f64() * 1000.0);
        if !identity.complete {
            return Err(format!("incomplete identity: {:?}", identity.incomplete_reason).into());
        }
        files = identity.files_hashed;
        hashes.push(identity.hash);
    }
    println!(
        "{}",
        serde_json::json!({"schemaVersion":1,"stage":"input-identity","fixture":path,
        "files":files,"samplesMs":times,"hashes":hashes,"warmup":3})
    );
    Ok(())
}
