use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use fs4::FileExt;
use serde_json::Value;

pub fn repo_root() -> Result<PathBuf> {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .context("xtask must live directly below the repository root")
}

pub fn publish(name: &str, run: &Value, results: &Value, report: &str) -> Result<PathBuf> {
    let root = repo_root()?
        .join("benchmark")
        .join("results")
        .join("stage7");
    let benchmark = root
        .parent()
        .and_then(Path::parent)
        .context("evidence root has no benchmark parent")?;
    let results_root = root
        .parent()
        .context("evidence root has no results parent")?;
    safe_directory(benchmark)?;
    safe_directory(results_root)?;
    safe_directory(&root)?;

    let lock_path = root.join(".publish.lock");
    reject_symlink_or_nonfile(&lock_path)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .with_context(|| format!("open evidence lock {}", lock_path.display()))?;
    FileExt::lock(&lock).context("lock evidence publication")?;

    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = root.join(format!(".{name}.tmp-{}-{suffix}", std::process::id()));
    fs::create_dir(&temporary)
        .with_context(|| format!("create temporary evidence entry {}", temporary.display()))?;

    let result = publish_locked(&temporary, name, run, results, report);
    if result.is_err() {
        remove_temporary(&temporary);
    }
    let _ = FileExt::unlock(&lock);
    result
}

fn publish_locked(
    temporary: &Path,
    name: &str,
    run: &Value,
    results: &Value,
    report: &str,
) -> Result<PathBuf> {
    let root = temporary
        .parent()
        .context("temporary evidence entry has no parent")?;
    let repository = repo_root()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let redacted_run = redact_value(run, &repository, home.as_deref());
    let redacted_results = redact_value(results, &repository, home.as_deref());
    let redacted_report = redact_text(report, &repository, home.as_deref());
    let run_bytes = json_bytes(&redacted_run)?;
    let result_bytes = json_bytes(&redacted_results)?;
    let report_bytes = redacted_report.as_bytes();
    let all_bytes = [run_bytes.as_slice(), result_bytes.as_slice(), report_bytes].concat();
    let all_text = String::from_utf8_lossy(&all_bytes);
    for forbidden in [
        repository.to_string_lossy().as_ref(),
        "sessionID",
        "session_id",
        "credential",
        "api_key",
        "authorization",
    ] {
        if all_text.contains(forbidden) {
            remove_temporary(temporary);
            bail!("evidence contains forbidden private text: {forbidden}");
        }
    }

    write_synced(&temporary.join("run.json"), &run_bytes)?;
    write_synced(&temporary.join("results.json"), &result_bytes)?;
    write_synced(&temporary.join("report.md"), report_bytes)?;
    sync_directory(temporary);

    let run_id = run
        .get("run_id")
        .and_then(Value::as_str)
        .context("run evidence must contain run_id")?;
    let suffix = temporary
        .file_name()
        .and_then(|value| value.to_str())
        .and_then(|value| value.rsplit('-').next())
        .context("temporary evidence entry has no unique suffix")?;
    let final_name = format!("{name}-{run_id}-{suffix}");
    let final_path = root.join(final_name);
    if final_path.exists() {
        remove_temporary(temporary);
        bail!("evidence output already exists: {}", final_path.display());
    }
    fs::rename(temporary, &final_path).with_context(|| {
        format!(
            "atomically publish evidence {} -> {}",
            temporary.display(),
            final_path.display()
        )
    })?;
    sync_directory(root);
    Ok(final_path)
}

fn json_bytes(value: &Value) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).context("serialize evidence JSON")?;
    bytes.push(b'\n');
    Ok(bytes)
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync {}", path.display()))?;
    Ok(())
}

fn sync_directory(path: &Path) {
    if let Ok(directory) = File::open(path) {
        let _ = directory.sync_all();
    }
}

fn remove_temporary(path: &Path) {
    let _ = fs::remove_dir_all(path);
}

fn safe_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("evidence directory is a symbolic link: {}", path.display())
        }
        Ok(metadata) if !metadata.is_dir() => {
            bail!("evidence path is not a directory: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path)
                .with_context(|| format!("create evidence directory {}", path.display()))?;
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("inspect evidence directory {}", path.display()))
        }
    }
}

fn reject_symlink_or_nonfile(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("evidence lock is a symbolic link: {}", path.display())
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!("evidence lock is not a regular file: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("inspect evidence lock {}", path.display()))
        }
    }
}

fn redact_value(value: &Value, repository: &Path, home: Option<&Path>) -> Value {
    match value {
        Value::String(text) => Value::String(redact_text(text, repository, home)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_value(value, repository, home))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), redact_value(value, repository, home)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn redact_text(text: &str, repository: &Path, home: Option<&Path>) -> String {
    let mut redacted = text.replace(repository.to_string_lossy().as_ref(), "<trial-root>");
    if let Some(home) = home {
        redacted = redacted.replace(home.to_string_lossy().as_ref(), "<home>");
    }
    redacted
}
