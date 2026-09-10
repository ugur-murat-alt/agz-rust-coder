//! Win32 path spellings for paths handed to platform build tools.

use std::path::{Path, PathBuf};

/// Return an ordinary (non-verbatim) Win32 spelling of `path` when one resolves
/// to the identical location, or `None` when the canonical spelling is the only
/// faithful form.
///
/// Cargo and rustc pass build-artifact paths to the platform linker, and the
/// MSVC `link.exe` cannot open `\\?\` verbatim paths. A canonical Windows path
/// must therefore be spelled without the prefix whenever that spelling resolves
/// back to the same file. Ordinary paths and non-Windows platforms are returned
/// unchanged.
#[cfg_attr(not(windows), allow(clippy::unnecessary_wraps))]
pub fn win32_spelling(path: &Path) -> Option<PathBuf> {
    #[cfg(not(windows))]
    {
        Some(path.to_owned())
    }
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};

        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Some(path.to_owned());
        };
        let Prefix::VerbatimDisk(drive) = prefix.kind() else {
            return Some(path.to_owned());
        };
        if components.next() != Some(Component::RootDir) {
            return None;
        }
        let mut argument = PathBuf::from(format!("{}:\\", char::from(drive)));
        for component in components {
            let Component::Normal(name) = component else {
                return None;
            };
            argument.push(name);
        }
        // Stripping a verbatim prefix is not generally semantics-preserving
        // (for example, trailing dots or reserved names). Require identical
        // resolution before the spelling is accepted.
        match std::fs::canonicalize(&argument) {
            Ok(resolved) if resolved == path => Some(argument),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_paths_are_returned_unchanged() {
        let path = std::env::temp_dir().join("agz-win32-spelling-fixture");
        assert_eq!(win32_spelling(&path), Some(path.clone()));
    }

    #[cfg(windows)]
    #[test]
    fn canonical_paths_get_an_identity_preserving_spelling() {
        let canonical = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp");
        assert!(
            canonical.as_os_str().to_string_lossy().starts_with(r"\\?\"),
            "canonical Windows paths are expected to be verbatim: {canonical:?}"
        );
        let spelling = win32_spelling(&canonical).expect("identity-preserving spelling");
        assert!(
            !spelling.as_os_str().to_string_lossy().starts_with(r"\\?\"),
            "the spelling must drop the verbatim prefix: {spelling:?}"
        );
        assert_eq!(
            std::fs::canonicalize(&spelling).expect("re-canonicalize"),
            canonical
        );
    }
}
