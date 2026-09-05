"""Apply narrowly scoped repairs to the immutable 0.2.0 candidate, not this branch."""
from pathlib import Path
import subprocess

BASE = 'c5d856ec332aad15fd01abba573771d8c798006f'
assert subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip() == BASE

def replace(name, old, new):
    path = Path(name)
    text = path.read_text(encoding='utf-8')
    assert text.count(old) == 1, (name, old, text.count(old))
    path.write_text(text.replace(old, new), encoding='utf-8', newline='\n')

metadata = 'crates/agz-rust-coder/src/workspace/metadata.rs'
replace(metadata, 'let root = std::env::temp_dir().join(format!(', 'let root = std::fs::canonicalize(std::env::temp_dir())\n                .expect("canonical temp directory")\n                .join(format!(')
replace(metadata, '    runtime: Handle,\n}', '    runtime: Handle,\n    supervised_sccache: bool,\n}')
replace(metadata, '            supervisor,\n            runtime,\n', '            supervisor,\n            runtime,\n            supervised_sccache: false,\n')
replace(metadata, '    pub(crate) fn checkpoint(&self) -> Result<(), MetadataError> {', '''    #[must_use]
    pub(crate) fn with_supervised_sccache(mut self, enabled: bool) -> Self {
        self.supervised_sccache = enabled;
        self
    }

    pub(crate) fn checkpoint(&self) -> Result<(), MetadataError> {''')
replace(metadata, 'let environment = std::env::vars_os().collect::<BTreeMap<_, _>>();', '''// An explicitly supervised sccache session starts only after metadata.
        // Metadata's rustc information probes must not start an ambient daemon.
        // Empty RUSTC_WRAPPER overrides Cargo config for this subprocess only;
        // compilation still uses the validated, owned wrapper session.
        let mut environment = std::env::vars_os().collect::<BTreeMap<_, _>>();
        if control.supervised_sccache {
            environment.insert("RUSTC_WRAPPER".into(), "".into());
        }''')
replace(metadata, '    graph_fingerprint: [u8; 32],\n}', '    graph_fingerprint: [u8; 32],\n    supervised_sccache: bool,\n}')
replace(metadata, '            graph_fingerprint,\n        };', '            graph_fingerprint,\n            supervised_sccache: control.is_some_and(|control| control.supervised_sccache),\n        };')
replace('crates/agz-rust-coder/src/tools/check.rs', '        );\n        let git = ControlledGitProbe::fixed(', '        )\n        .with_supervised_sccache(request.options.sccache);\n        let git = ControlledGitProbe::fixed(')

supervisor = 'crates/agz-rust-coder/src/process/supervisor.rs'
replace(supervisor, '            match child.try_wait() {', '''            // process-wrap 10's JobObject::try_wait consumes completion-port
            // events that its final wait also needs. Poll only the leader here;
            // retain the outer job for tree termination and the final wait.
            #[cfg(windows)]
            let polled = child.inner_mut().try_wait();
            #[cfg(not(windows))]
            let polled = child.try_wait();
            match polled {''')

atomic = 'crates/agz-rust-coder/src/cache/atomic.rs'
replace(atomic, '            Component::Prefix(prefix) => current.push(prefix.as_os_str()),', '''            Component::Prefix(prefix) => {
                current.push(prefix.as_os_str());
                // A drive prefix is not a complete directory until RootDir.
                continue;
            }''')
replace(atomic, '    let current = open_directory(&directory.path)?;', '    let current = open_directory(&directory.path)?;\n    #[cfg(not(unix))]\n    let _ = &current;')
lease = 'crates/agz-rust-coder/src/gate/lease.rs'
replace(lease, '        current.push(component.as_os_str());\n        match fs::symlink_metadata(&current) {', '''        if matches!(component, std::path::Component::ParentDir) {
            return false;
        }
        current.push(component.as_os_str());
        if matches!(component, std::path::Component::Prefix(_)) {
            continue;
        }
        match fs::symlink_metadata(&current) {''')
replace(lease, '''    #[cfg(unix)]
    {
        PathBuf::from(format!("/proc/{pid}/stat")).is_file()
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }''', '''    if pid == std::process::id() {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        // An unreadable process is not evidence of a dead owner.
        match fs::metadata(format!("/proc/{pid}")) {
            Ok(_) => true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(_) => true,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Without a verified OS liveness probe, fail closed instead of stealing
        // a live foreign process's lease. Manual recovery may be required.
        true
    }''')

identity = 'crates/agz-rust-coder/src/process/identity.rs'
replace(identity, '    fs, io,', '    io,')
replace(identity, 'use sha2::{Digest, Sha256};', '#[cfg(target_os = "linux")]\nuse std::fs;\n\nuse sha2::{Digest, Sha256};')
replace(identity, '    #[error("malformed process identity: {0}")]', '    #[cfg(target_os = "linux")]\n    #[error("malformed process identity: {0}")]')
replace('xtask/src/child_process.rs', '#[cfg(test)]\nmod tests {', '#[cfg(all(test, target_os = "linux"))]\nmod tests {')
replace('xtask/src/child_process.rs', '    #[cfg(target_os = "linux")]\n    #[tokio::test]', '    #[tokio::test]')
replace('crates/agz-rust-coder/src/docs/cache.rs', '    let opened = directory\n', '    let opened = directory\n')  # existence assertion
replace('crates/agz-rust-coder/src/docs/cache.rs', '    #[cfg(unix)]\n    {\n        use std::os::unix::fs::MetadataExt as _;', '    #[cfg(not(unix))]\n    let _ = &opened;\n    #[cfg(unix)]\n    {\n        use std::os::unix::fs::MetadataExt as _;')

path = Path('crates/agz-rust-coder/tests/check_service.rs')
text = path.read_text()
begin = text.index('#[tokio::test]\nasync fn a_new_input_supersedes_the_older_active_job()')
end = text.index('#[tokio::test]\nasync fn streamed_error_has_request_timing', begin)
text = text[:begin] + r'''#[tokio::test]
async fn a_new_input_supersedes_the_older_active_job() {
    let project = TestProject::new("supersede", "pub fn value() -> usize { 1 }\n", None);
    let ready = project.state.join("build-ready");
    let release = project.state.join("build-release");
    // Synchronize with actual Cargo execution, not preflight wall-clock speed.
    fs::write(project.root.join("build.rs"), format!(
        "fn main() {{ std::fs::write({:?}, b\"ready\").unwrap(); let start = std::time::Instant::now(); while !std::path::Path::new({:?}).exists() {{ assert!(start.elapsed() < std::time::Duration::from_secs(45), \"test barrier expired\"); std::thread::sleep(std::time::Duration::from_millis(10)); }} }}\n",
        ready, release,
    )).expect("write bounded build barrier");
    let service = project.service();
    let first = {
        let service = Arc::clone(&service);
        let root = project.root.clone();
        tokio::spawn(async move {
            service.run(GateRequest::new(root, GateTargetId::Check), None, None).await
        })
    };
    let started = tokio::time::timeout(Duration::from_secs(30), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await;
    if started.is_err() {
        fs::write(&release, b"release").expect("release failed fixture");
        service.close().await;
        panic!("first Cargo build never reached the synchronization barrier");
    }
    fs::write(project.root.join("src/lib.rs"), "pub fn value() -> usize { 2 }\n")
        .expect("write new source generation");
    let second = {
        let service = Arc::clone(&service);
        let root = project.root.clone();
        tokio::spawn(async move {
            service.run(GateRequest::new(root, GateTargetId::Check), None, None).await
        })
    };
    let first = tokio::time::timeout(Duration::from_secs(20), first).await;
    fs::write(&release, b"release").expect("release replacement build");
    let second = tokio::time::timeout(Duration::from_secs(30), second).await;
    service.close().await;
    let first = first.expect("older check superseded within its bound").expect("join older check");
    let second = second.expect("replacement check completed within its bound").expect("join replacement");
    assert_eq!(first.status, GateStatus::Superseded, "{first:#?}");
    assert_eq!(second.status, GateStatus::FastPass, "{second:#?}");
    assert_ne!(first.input_hash, second.input_hash);
}

''' + text[end:]
anchor = 'async fn real_sccache_compiles_and_cleans_the_owned_server() {\n'
assert text.count(anchor) == 1
text = text.replace(anchor, anchor + '    #[cfg(target_os = "linux")]\n    let before = live_sccache_pids();\n')
begin = text.index(anchor)
end = text.index('\n#[tokio::test]\nasync fn filtered_cargo', begin)
section = text[begin:end]
assert section.count('    service.close().await;\n}') == 1
section = section.replace('    service.close().await;\n}', '''    service.close().await;
    #[cfg(target_os = "linux")]
    tokio::time::timeout(Duration::from_secs(5), async {
        while !live_sccache_pids().is_subset(&before) {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }).await.expect("sccache left a newly started live process outside its owned session");
}''')
text = text[:begin] + section + text[end:]
text += '''
#[cfg(target_os = "linux")]
fn live_sccache_pids() -> std::collections::BTreeSet<u32> {
    let wrapper = fs::canonicalize(std::env::var_os("RUSTC_WRAPPER")
        .expect("explicit sccache wrapper")).expect("canonical sccache binary");
    fs::read_dir("/proc").expect("read process table")
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<u32>().ok()?;
            if fs::read_link(entry.path().join("exe")).ok()? != wrapper {
                return None;
            }
            let stat = fs::read_to_string(entry.path().join("stat")).ok()?;
            let (_, fields) = stat.rsplit_once(')')?;
            (fields.split_whitespace().next() != Some("Z")).then_some(pid)
        }).collect()
}
'''
path.write_text(text, encoding='utf-8', newline='\n')

Path('crates/agz-rust-coder/tests/release_portability.rs').write_text(r'''use std::{fs, time::{Duration, SystemTime, UNIX_EPOCH}};
use agz_rust_coder::gate::lease::{acquire_lease_with_timeout, LeaseError};

#[tokio::test]
async fn a_live_lease_is_not_reclaimed_and_release_allows_reacquisition() {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).expect("clock").as_nanos();
    let root = fs::canonicalize(std::env::temp_dir()).expect("canonical temp")
        .join(format!("agz-lease-regression-{}-{stamp}", std::process::id()));
    fs::create_dir(&root).expect("create lease directory");
    let mut first = acquire_lease_with_timeout(&root, "same-key", Duration::from_secs(2), None)
        .await.expect("first lease");
    let second = acquire_lease_with_timeout(&root, "same-key", Duration::from_millis(200), None).await;
    assert!(matches!(second, Err(LeaseError::TimedOut { .. })), "live lease was stolen: {second:?}");
    first.release();
    let mut third = acquire_lease_with_timeout(&root, "same-key", Duration::from_secs(2), None)
        .await.expect("lease after release");
    third.release();
    fs::remove_dir_all(root).expect("remove lease fixture");
}

#[cfg(windows)]
#[tokio::test]
async fn windows_job_can_be_reaped_after_leader_polling() {
    use agz_rust_coder::process::{ProcessRunOptions, ProcessSupervisor};
    let supervisor = ProcessSupervisor::without_journal();
    let executable = std::env::var_os("ComSpec").expect("Windows command processor");
    let cwd = fs::canonicalize(std::env::temp_dir()).expect("canonical cwd");
    for _ in 0..20 {
        let result = tokio::time::timeout(Duration::from_secs(5), supervisor.run(
            std::path::PathBuf::from(&executable), ["/D", "/C", "exit 0"],
            ProcessRunOptions::new(&cwd).with_environment(std::env::vars_os())
                .with_timeout(Duration::from_secs(2)),
        )).await.expect("bounded completed job").expect("launch completed job");
        assert_eq!(result.exit_code, 0, "{result:?}");
        assert!(result.cleanup_complete && result.drain_complete, "{result:?}");
        assert_eq!(supervisor.active_count(), 0);
    }
    assert_eq!(supervisor.close().await.remaining, 0);
}
''', encoding='utf-8', newline='\n')

step = '\n'.join([
    '      - name: Install smoke server outside authorized source roots',
    '        shell: bash',
    '        run: |',
    '          cargo +1.88.0 build -p agz-rust-coder --locked',
    "          python3 - <<'PYTHON'",
    '          import os, pathlib, shutil, tempfile',
    "          suffix = '.exe' if os.name == 'nt' else ''",
    "          directory = pathlib.Path(tempfile.mkdtemp(prefix='agz-smoke-', dir=os.environ['RUNNER_TEMP'])).resolve()",
    "          source = pathlib.Path('target/debug') / ('agz-rust-coder' + suffix)",
    '          destination = directory / source.name',
    '          shutil.copy2(source, destination)',
    '          destination.chmod(0o755)',
    "          with open(os.environ['GITHUB_ENV'], 'a', encoding='utf-8') as environment:",
    "              environment.write('AGZ_RUST_CODER_BIN=' + str(destination) + '\\n')",
    '          PYTHON', '',
    '      - name: Run protocol smoke', '',
])
replace('.github/workflows/ci.yml', '      - name: Run protocol smoke\n', step)
replace('CHANGELOG.md', '- Corrected aggregate diagnostic timing, completed-stage counts, failed-test\n  tails, zero-test matches and workspace doctest selection.', '- Corrected aggregate diagnostic timing, completed-stage counts, failed-test\n  tails, zero-test matches and workspace doctest selection.\n- Prevented metadata compiler-information probes from starting an unmanaged\n  sccache daemon before an explicitly requested supervised cache session.\n- Corrected Windows drive-prefix checks and job-completion polling, preserved\n  live host leases on macOS/Windows, and fixed canonical-path/timing fixtures.\n- Installed smoke binaries outside authorized source roots without weakening\n  the root-bound executable check.')
for name, note in [
    ('docs/tools.md', '\nOn macOS and Windows, host leases with an unverifiable foreign PID are retained\nrather than reclaimed. After a confirmed owner crash, an operator may need to\nremove that stale lease while no validation process is using it. Linux retains\nverified absent-PID recovery. With opt-in Sccache, metadata probes bypass only\nRUSTC_WRAPPER; compilation still uses the owned, validated cache session.\n'),
    ('docs/tools.tr.md', '\nmacOS ve Windows üzerinde başka bir sürece ait olduğu görülen, fakat sahibinin\nsonlandığı doğrulanamayan host lease dosyaları silinmez. Sahip sürecin çöktüğü\ndoğrulandıktan ve doğrulama işlemi çalışmadığından emin olunduktan sonra eski\nlease dosyasını elle temizlemek gerekebilir. Linux üzerinde PID yokluğu\ndoğrulanarak kurtarma korunur. Sccache açıkken yalnız metadata sorgularında\nRUSTC_WRAPPER devre dışıdır; derleme yönetilen ve doğrulanmış cache oturumunu kullanır.\n'),
]:
    p = Path(name)
    p.write_text(p.read_text() + note, encoding='utf-8', newline='\n')
print('Prepared targeted repairs; validation is still required before publication.')
