"""Repair cross-platform path representations and test transports; retain all gates."""
from pathlib import Path


def replace(name, old, new):
    p = Path(name)
    text = p.read_text(encoding='utf-8')
    assert text.count(old) == 1, (name, old, text.count(old))
    p.write_text(text.replace(old, new), encoding='utf-8', newline='\n')


base = 'crates/agz-rust-coder/'
p = Path(base + 'src/workspace/roots.rs')
text = p.read_text()
start = text.index('fn normalize_path(path: &Path)')
end = text.index('\nfn normalize_relative(', start)
part = text[start:end]
old = '            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),'
assert part.count(old) == 1
part = part.replace(old, r'''            Component::Prefix(prefix) => {
                // Cargo reports ordinary drive paths while canonical roots use
                // verbatim drive prefixes. Normalize syntax only: never resolve
                // symlinks or remove parent components before authorization.
                #[cfg(windows)]
                if path.is_absolute()
                    && let Prefix::Disk(drive) = prefix.kind()
                {
                    normalized.push(format!(r"\\?\{}:", char::from(drive)));
                    continue;
                }
                normalized.push(prefix.as_os_str());
            }''')
p.write_text(text[:start] + part + text[end:])
name = base + 'tests/lsp_manager.rs'
replace(name, '    let protocol_root = PathBuf::from("/agz-stable-lsp-root");',
        '    let protocol_root = root.path().with_extension("protocol-alias");')
replace(name, '''    fs::rename(root.path(), &original).expect("move lexical root after initialization");
    fs::create_dir(root.path()).expect("create lexical replacement");''',
        '''    #[cfg(unix)]
    {
        fs::rename(root.path(), &original).expect("move lexical root after initialization");
        fs::create_dir(root.path()).expect("create lexical replacement");
    }
    #[cfg(windows)]
    {
        assert!(fs::rename(root.path(), &original).is_err(), "authorized root must remain pinned");
        assert!(root.path().is_dir());
        assert!(!original.exists());
    }''')
replace(name, '    let expected_uri = "file:///agz-stable-lsp-root";',
        '    let expected_uri = lsp::path_to_file_uri(&protocol_root).expect("absolute protocol URI");')
replace(name, '        format!("file://{}", root.path().display()),',
        '        lsp::path_to_file_uri(root.path()).expect("absolute lexical URI"),')
replace(name, '    fs::remove_dir_all(&original).expect("remove original root");',
        '    #[cfg(unix)]\n    fs::remove_dir_all(&original).expect("remove original root");')
name = base + 'tests/lsp_navigation.rs'
p = Path(name)
text = p.read_text()
start = text.index('    fs::rename(root.path(), &old).expect("rename original root");')
end = text.index('\n}\n', start)
part = text[start:end]
assert part.endswith('    old')
new = '''    #[cfg(unix)]
    {
''' + ''.join('    ' + line + '\n' for line in part.splitlines()) + '''    }
    #[cfg(windows)]
    {
        assert!(fs::rename(root.path(), &old).is_err(), "Windows must reject replacement while authority is retained");
        assert!(!old.exists());
        assert!(fs::read_to_string(root.path().join("src/lib.rs")).expect("pinned source").contains("pub fn mock_fn"));
        fs::write(root.path().join(".semantic-ra-continue"), "continue\\n").expect("release pinned semantic fixture");
        old
    }'''
text = text[:start] + new + text[end:]
old = '    assert!(rename.reason.contains("context src/lib.rs"));'
assert text.count(old) == 1
text = text.replace(old, '''    let context_label = format!("context {}", Path::new("src").join("lib.rs").display());
    assert!(rename.reason.contains(&context_label), "{}", rename.reason);''')
old = '''    assert_eq!(
        fs::read_to_string(root.path().join("src/lib.rs")).expect("read replacement source"),
        "pub fn replacement_only() { panic!(\\"replacement content\\") }\\n"
    );'''
assert text.count(old) == 1
text = text.replace(old, '    #[cfg(unix)]\n' + old + '''
    #[cfg(windows)]
    assert_eq!(
        fs::read(root.path().join("src/lib.rs")).expect("read pinned source"),
        original_source,
        "semantic operations must not modify the pinned source"
    );''')
anchor = '    let root = TestRoot::new("retained-edits");'
assert text.count(anchor) == 1
text = text.replace(anchor, anchor + '''
    #[cfg(windows)]
    let original_source = fs::read(root.path().join("src/lib.rs")).expect("snapshot original source");''')
p.write_text(text)
name = 'tests/fixtures/lsp/semantic_ra.rs'
replace(name, '    let uri = json_string(uri);\n    match method {',
        '''    let uri = json_string(uri);
    let outside_uri = if cfg!(windows) { "file:///C:/agz-outside/other.rs" } else { "file:///mock/other.rs" };
    match method {''')
replace(name, '"uri":"file:///mock/other.rs"', '"uri":"{outside_uri}"')
name = base + 'tests/workspace_selection.rs'
replace(name, '''    assert!(matches!(
        root_snapshot.select(Some(&root.path().join("../outside"))),
        Err(RootError::ParentComponent)
    ));''', r'''    let mut raw = root.path().as_os_str().to_owned();
    raw.push(std::path::MAIN_SEPARATOR_STR);
    raw.push("..");
    raw.push(std::path::MAIN_SEPARATOR_STR);
    raw.push("outside");
    let parent_escape = PathBuf::from(raw);
    assert!(parent_escape.components().any(|component| component == std::path::Component::ParentDir));
    assert!(matches!(
        root_snapshot.select(Some(&parent_escape)),
        Err(RootError::ParentComponent)
    ));''')
name = base + 'tests/protocol_tasks.rs'
replace(name, '        config.docs.timeout_ms = 30_000;', '        config.docs.timeout_ms = 60_000;')
replace(name, 'std::thread::sleep(std::time::Duration::from_secs(10));', 'std::thread::sleep(std::time::Duration::from_secs(60));')
replace(name, '''    let mut build_pid = None;
    for _ in 0..500 {''',
        '''    let mut build_pid = None;
    let startup_deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::time::Instant::now() < startup_deadline {''')
replace(name, '    let build_pid = build_pid.context("local cargo doc did not start its build script")?;',
        '''    let current = client.peer().get_task(GetTaskParams::new(task.task.task_id.clone())).await?;
    let Some(build_pid) = build_pid else {
        client.cancel().await?;
        server.await??;
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&cache);
        anyhow::bail!("local cargo doc did not reach its build barrier; task={current:?}");
    };''')
name = 'xtask/src/opencode.rs'
replace(name, '''fn read_http_request(stream: &mut TcpStream) -> std::io::Result<HttpRequest> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;''',
        '''fn read_http_request(stream: &mut TcpStream) -> std::io::Result<HttpRequest> {
    // accept() may inherit the listener's nonblocking flag on BSD/macOS.
    // Explicit blocking mode keeps fragmented requests under the read timeout.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;''')
p = Path(name)
text = p.read_text()
anchor = '#[cfg(test)]\nmod tests {'
assert text.count(anchor) == 1
text = text.replace(anchor, anchor + r'''
    #[test]
    fn fake_provider_reads_fragmented_requests_from_nonblocking_accepted_sockets() {
        use super::*;
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        let mut client = TcpStream::connect(listener.local_addr().expect("fixture address")).expect("connect fixture");
        let (mut accepted, _) = listener.accept().expect("accept fixture");
        accepted.set_nonblocking(true).expect("reproduce inherited socket mode");
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reader = thread::spawn(move || {
            ready_tx.send(()).expect("signal reader");
            read_http_request(&mut accepted)
        });
        ready_rx.recv_timeout(Duration::from_secs(1)).expect("reader started");
        thread::sleep(Duration::from_millis(30));
        client.write_all(b"POST / HTTP/1.1\r\nContent-Length: 2\r\n").expect("partial headers");
        thread::sleep(Duration::from_millis(30));
        client.write_all(b"\r\n{").expect("partial body");
        thread::sleep(Duration::from_millis(30));
        client.write_all(b"}").expect("body end");
        let request = reader.join().expect("reader joined").expect("bounded request read");
        assert_eq!(request.body, b"{}");
    }
''')
p.write_text(text)
p = Path(base + 'tests/windows_path_identity.rs')
assert not p.exists()
p.write_text(r'''#![cfg(windows)]
use agz_rust_coder::workspace::{ClientRoots, RootError, RootGuard};
use std::{fs, path::PathBuf, time::SystemTime};

#[test]
fn ordinary_and_verbatim_drive_paths_share_authority_without_expanding_it() {
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("clock").as_nanos();
    let base = fs::canonicalize(std::env::temp_dir()).expect("canonical temporary directory")
        .join(format!("agz-path-identity-{}-{stamp}", std::process::id()));
    let root = base.join("root");
    let sibling = base.join("root-sibling");
    fs::create_dir_all(root.join("child")).expect("create root");
    fs::create_dir_all(&sibling).expect("create sibling");
    fs::write(root.join("child/source.rs"), "pub fn item() {}\n").expect("write source");
    let guard = RootGuard::new([root.clone()], std::iter::empty()).expect("root guard");
    let authority = guard.configured_roots()[0].clone();
    let plain = PathBuf::from(root.to_str().expect("UTF-8 root").strip_prefix(r"\\?\").expect("verbatim drive"));
    assert!(authority.contains(&plain));
    assert_eq!(authority.authorize_dir(&plain.join("child")).expect("ordinary authorized child").path(), root.join("child"));
    let snapshot = guard.snapshot(ClientRoots::unsupported()).expect("snapshot");
    assert!(snapshot.select(Some(&plain)).is_ok());
    assert!(!authority.contains(&sibling));
    assert!(authority.authorize_dir(&sibling).is_err());
    let escape = PathBuf::from(format!("{}\\..\\root-sibling", plain.display()));
    assert!(matches!(snapshot.select(Some(&escape)), Err(RootError::ParentComponent)));
    drop(snapshot);
    drop(authority);
    drop(guard);
    fs::remove_dir_all(base).expect("remove fixture");
}
''')
print('Applied bounded transport, drive-identity and platform-fixture repairs.')
