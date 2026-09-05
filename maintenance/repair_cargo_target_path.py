"""Apply after the portable-contract and fixture-bound scripts."""
from pathlib import Path


def replace(name, old, new):
    p = Path(name)
    text = p.read_text(encoding='utf-8')
    assert text.count(old) == 1, (name, old, text.count(old))
    p.write_text(text.replace(old, new), encoding='utf-8', newline='\n')


name = 'crates/agz-rust-coder/src/docs/resolver.rs'
replace(name, '        let cargo = crate::tools::check::resolve_cargo(None);',
'''        // Retain the canonical path for authorization and output reads. Only
        // the Cargo command argument uses a verified equivalent Win32 spelling.
        #[cfg(windows)]
        let target_argument = windows_cargo_target_argument(&target)?;
        #[cfg(not(windows))]
        let target_argument = target.clone();
        let cargo = crate::tools::check::resolve_cargo(None);''')
replace(name, '            target.as_os_str().to_owned(),', '            target_argument.as_os_str().to_owned(),')
replace(name, 'fn collect_generated_pages(root: &Path, deadline: Instant) -> Result<Vec<GeneratedPage>, String> {', r'''#[cfg(windows)]
fn windows_cargo_target_argument(target: &Path) -> Result<PathBuf, String> {
    use std::path::{Component, Prefix};

    let mut components = target.components();
    let Some(Component::Prefix(prefix)) = components.next() else {
        return Err("canonical Cargo output path has no Windows prefix".to_owned());
    };
    let Prefix::VerbatimDisk(drive) = prefix.kind() else {
        return Ok(target.to_owned());
    };
    if components.next() != Some(Component::RootDir) {
        return Err("canonical Cargo output path is not absolute".to_owned());
    }
    let mut argument = PathBuf::from(format!("{}:\\", char::from(drive)));
    for component in components {
        let Component::Normal(name) = component else {
            return Err("canonical Cargo output path contains traversal".to_owned());
        };
        argument.push(name);
    }
    // Stripping a verbatim prefix is not generally semantics-preserving (for
    // example, trailing dots or reserved names). Require identical resolution.
    let resolved = fs::canonicalize(&argument).map_err(|error| error.to_string())?;
    if resolved != target {
        return Err("Cargo output path has no identity-preserving Win32 spelling".to_owned());
    }
    Ok(argument)
}

fn collect_generated_pages(root: &Path, deadline: Instant) -> Result<Vec<GeneratedPage>, String> {''')
# Keep the original, previously failing long fixture names. The measured cause
# was the verbatim argument, not path length; do not hide it with a short fixture.
name = 'crates/agz-rust-coder/tests/protocol_tasks.rs'
replace(name, '        "agz-d-{}-{stamp:x}",', '        "agz-rust-coder-docs-task-{}-{stamp}",')
replace(name, '        "agz-c-{}-{stamp:x}",', '        "agz-rust-coder-docs-task-cache-{}-{stamp}",')
replace(name, '''    // This test checks cancellation, not the linker's maximum output path.
    // Leave room for the cache digest and Cargo's build-script subdirectories.
''', '''    // Preserve the deep canonical cache path: this also regresses MSVC
    // rejection of a verbatim Cargo --target-dir argument.
''')
print('Cargo gets an identity-checked Windows target argument; canonical guards remain unchanged.')
