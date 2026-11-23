//! Which workspace directories are throwaway: gone from disk, or inside a temp root.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const FIXED_ROOTS: [&str; 4] = [
    "/tmp",
    "/private/tmp",
    "/var/folders",
    "/private/var/folders",
];

/// Pure rule: a missing directory, or one at or under any of `temp_roots`.
pub fn is_throwaway(path: &Path, exists: bool, temp_roots: &[PathBuf]) -> bool {
    !exists || temp_roots.iter().any(|r| path.starts_with(r))
}

/// Applies [`is_throwaway`] against the real file system and the process temp dir.
pub fn is_throwaway_dir(path: &Path) -> bool {
    static ROOTS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    let roots = ROOTS.get_or_init(|| {
        let tmp = std::env::temp_dir();
        let mut roots: Vec<PathBuf> = FIXED_ROOTS.iter().map(PathBuf::from).collect();
        roots.extend(tmp.canonicalize().ok());
        roots.push(tmp);
        roots
    });
    match path.canonicalize() {
        Ok(real) => is_throwaway(&real, true, roots) || is_throwaway(path, true, roots),
        Err(_) => true,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn roots() -> Vec<PathBuf> {
        vec!["/tmp".into(), "/private/var/folders".into()]
    }

    #[test]
    fn missing_dirs_are_throwaway() {
        assert!(is_throwaway(Path::new("/Users/me/gone"), false, &roots()));
    }

    #[test]
    fn dirs_under_a_temp_root_are_throwaway() {
        assert!(is_throwaway(Path::new("/tmp"), true, &roots()));
        assert!(is_throwaway(Path::new("/tmp/tmp.abc"), true, &roots()));
        assert!(is_throwaway(
            Path::new("/private/var/folders/x/T/aw-claude-live-1"),
            true,
            &roots()
        ));
    }

    #[test]
    fn existing_dirs_outside_temp_are_kept() {
        assert!(!is_throwaway(Path::new("/Users/me/proj"), true, &roots()));
        // Component-wise: a sibling sharing a prefix is not under the root.
        assert!(!is_throwaway(Path::new("/tmpfoo/x"), true, &roots()));
        assert!(!is_throwaway(Path::new("/Users/me/proj"), true, &[]));
    }

    #[test]
    fn real_fs_checks() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_throwaway_dir(dir.path()), "temp dir");
        assert!(is_throwaway_dir(&dir.path().join("missing")), "missing");
        assert!(!is_throwaway_dir(Path::new(env!("CARGO_MANIFEST_DIR"))));
    }
}
