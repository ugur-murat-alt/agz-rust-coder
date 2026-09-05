"""Complete platform-specific lint corrections without changing validation policy."""
from pathlib import Path


def replace(name, old, new):
    path = Path(name)
    text = path.read_text(encoding='utf-8')
    assert text.count(old) == 1, (name, old, text.count(old))
    path.write_text(text.replace(old, new), encoding='utf-8', newline='\n')


base = 'crates/agz-rust-coder/'
# The shared APIs are genuinely fallible on their supported Unix/Linux paths.
# Retain those signatures in platform stubs instead of changing callers by OS.
for name, signature, condition in [
    ('src/gate/scheduler.rs', 'fn available_memory_bytes() -> std::io::Result<Option<u64>> {', 'not(target_os = "linux")'),
    ('src/lsp/manager.rs', 'fn authorities_match(left: &AuthorizedRoot, right: &AuthorizedRoot) -> Result<bool, ManagerError> {', 'not(unix)'),
    ('src/process/journal.rs', 'fn set_private_file_mode(file: &File, path: &Path) -> Result<(), JournalError> {', 'not(unix)'),
    ('src/telemetry.rs', 'fn set_private_file_permissions(_file: &File) -> io::Result<()> {', 'not(unix)'),
    ('src/telemetry.rs', 'fn set_private_directory_permissions(_path: &Path) -> io::Result<()> {', 'not(unix)'),
]:
    replace(base + name, signature,
        '// Preserve the shared fallible interface on platforms with a no-op implementation.\n'
        + '#[cfg_attr(' + condition + ', allow(clippy::unnecessary_wraps))]\n' + signature)
replace(base + 'src/process/journal.rs',
    'fn set_private_file_mode(file: &File, path: &Path) -> Result<(), JournalError> {\n',
    'fn set_private_file_mode(file: &File, path: &Path) -> Result<(), JournalError> {\n    #[cfg(not(unix))]\n    let _ = (file, path);\n')
replace(base + 'src/process/supervisor.rs', 'const SIGTERM: i32 = 15;', '#[cfg(unix)]\nconst SIGTERM: i32 = 15;')
path = Path(base + 'src/server/handler.rs')
text = path.read_text(encoding='utf-8')
start = text.index('                "definition" => {\n                    with_lsp_authority(')
end = text.index('                _ => unreachable!("validated semantic tool"),', start)
part = text[start:end]
assert part.count('with_lsp_authority(') == 1 and part.count('                    )\n                    .await') == 1
part = part.replace('with_lsp_authority(', 'Box::pin(with_lsp_authority(')
part = part.replace('                    )\n                    .await', '                    ))\n                    .await')
path.write_text(text[:start] + part + text[end:], encoding='utf-8', newline='\n')
replace(base + 'tests/check_service.rs', '    fn service_with_cargo(&self, cargo: PathBuf) -> Arc<CheckService> {', '    #[cfg(unix)]\n    fn service_with_cargo(&self, cargo: PathBuf) -> Arc<CheckService> {')
replace(base + 'tests/docs.rs', '#[derive(Debug)]\nstruct RecordingGenerator', '#[cfg(unix)]\n#[derive(Debug)]\nstruct RecordingGenerator')
replace(base + 'tests/docs.rs', 'impl LocalDocGenerator for RecordingGenerator {', '#[cfg(unix)]\nimpl LocalDocGenerator for RecordingGenerator {')
replace(base + 'tests/audit.rs', '    AuditSkipReason, AuditSummary, InvalidPathReason,', '    AuditSkipReason, AuditSummary,')
replace(base + 'tests/audit.rs', 'use agz_rust_coder::workspace::{ClientRoots, RootGuard, WorkspaceRoot};', '#[cfg(unix)]\nuse agz_rust_coder::tools::audit::InvalidPathReason;\nuse agz_rust_coder::workspace::{ClientRoots, RootGuard, WorkspaceRoot};')
replace(base + 'tests/security_roots.rs', 'ClientRoots, RootError, RootGuard, WalkIssueKind, WalkLimits, WorkspaceRoot, parse_file_uri,', 'ClientRoots, RootError, RootGuard, WalkLimits, WorkspaceRoot, parse_file_uri,')
replace(base + 'tests/security_roots.rs', 'struct TestDir(PathBuf);', '#[cfg(unix)]\nuse agz_rust_coder::workspace::WalkIssueKind;\n\nstruct TestDir(PathBuf);')
print('Platform lint corrections applied; full tests and Clippy remain required.')
