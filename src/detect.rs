//! Language detection for zero-config Nix environments.
//!
//! A project's env is composed from named prologues (see `nixenv::PROLOGUES`). Today a project
//! defaults to `[ "base" ]` and the user opts into a language prologue by hand. This module
//! infers the right prologues from the working tree so `tup run --env nix <project>` works on a
//! freshly-dropped Rust/Python/JS/… project with no config.
//!
//! `detect_prologues` is a pure function of the filesystem: it scans the project root (top
//! level, plus one cheap level into immediate subdirectories — monorepo/`src`-style layouts)
//! for well-known marker files and returns the matching prologue names, in a deterministic
//! order (the `MARKERS` table order), each at most once.

use std::path::{Path, PathBuf};

/// Marker file → prologue name. Order here is the order detections are reported in.
const MARKERS: &[(&str, &str)] = &[
    ("Cargo.toml", "rust"),
    ("go.mod", "go"),
    ("package.json", "node"),
    ("pyproject.toml", "python"),
    ("requirements.txt", "python"),
    ("setup.py", "python"),
    ("Pipfile", "python"),
    ("lean-toolchain", "lean"),
    ("lakefile.lean", "lean"),
];

/// Immediate subdirectories never worth descending into: build/dep caches that can contain
/// stray marker files (e.g. a `package.json` inside `node_modules`) and are large to walk.
/// Hidden dirs (`.git`, `.venv`, …) are skipped separately.
const SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "result",
];

/// The directories to scan: the root plus its immediate, non-skipped subdirectories (sorted for
/// determinism). One level only — cheap, and enough for `src/`-style and small monorepo layouts.
fn scan_dirs(root: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![root.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(root) {
        let mut subs: Vec<PathBuf> = rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.path())
            .filter(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                !name.starts_with('.') && !SKIP_DIRS.contains(&name)
            })
            .collect();
        subs.sort();
        dirs.extend(subs);
    }
    dirs
}

/// Detect prologues with the marker file that triggered each one (the first marker, in table
/// order, found in any scanned directory). Used for the human-facing "detected: rust
/// (Cargo.toml)" message.
pub(crate) fn detect_with_markers(root: &Path) -> Vec<(&'static str, &'static str)> {
    let dirs = scan_dirs(root);
    let mut out: Vec<(&'static str, &'static str)> = Vec::new();
    for &(marker, prologue) in MARKERS {
        if out.iter().any(|(p, _)| *p == prologue) {
            continue; // already detected via an earlier marker
        }
        if dirs.iter().any(|d| d.join(marker).is_file()) {
            out.push((prologue, marker));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The prologue names alone (no triggering markers), deduplicated and in `MARKERS` order.
    fn detect_prologues(root: &Path) -> Vec<&'static str> {
        detect_with_markers(root).into_iter().map(|(p, _)| p).collect()
    }

    /// A throwaway temp dir (no external crate; libc is the only dependency).
    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let mut p = std::env::temp_dir();
            p.push(format!("tup-detect-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            TmpDir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn touch(&self, rel: &str) {
            let f = self.0.join(rel);
            if let Some(parent) = f.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&f, b"").unwrap();
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn empty_tree_detects_nothing() {
        let d = TmpDir::new("empty");
        assert!(detect_prologues(d.path()).is_empty());
    }

    #[test]
    fn detects_rust_from_cargo_toml() {
        let d = TmpDir::new("rust");
        d.touch("Cargo.toml");
        assert_eq!(detect_prologues(d.path()), vec!["rust"]);
    }

    #[test]
    fn detects_each_language_by_its_markers() {
        for (marker, prologue) in [
            ("go.mod", "go"),
            ("package.json", "node"),
            ("pyproject.toml", "python"),
            ("requirements.txt", "python"),
            ("setup.py", "python"),
            ("Pipfile", "python"),
            ("lean-toolchain", "lean"),
            ("lakefile.lean", "lean"),
        ] {
            let d = TmpDir::new("one");
            d.touch(marker);
            assert_eq!(
                detect_prologues(d.path()),
                vec![prologue],
                "marker {marker} should map to {prologue}"
            );
        }
    }

    #[test]
    fn multiple_languages_dedup_and_order() {
        let d = TmpDir::new("multi");
        // Create out of table order; result must follow MARKERS order: rust, node, python.
        d.touch("package.json");
        d.touch("pyproject.toml");
        d.touch("Cargo.toml");
        assert_eq!(detect_prologues(d.path()), vec!["rust", "node", "python"]);
    }

    #[test]
    fn python_markers_collapse_to_one_entry() {
        let d = TmpDir::new("py-multi");
        d.touch("pyproject.toml");
        d.touch("requirements.txt");
        d.touch("setup.py");
        assert_eq!(detect_prologues(d.path()), vec!["python"]);
    }

    #[test]
    fn descends_one_level_into_subdirs() {
        let d = TmpDir::new("sub");
        d.touch("backend/go.mod");
        d.touch("frontend/package.json");
        assert_eq!(detect_prologues(d.path()), vec!["go", "node"]);
    }

    #[test]
    fn skips_heavy_and_hidden_dirs() {
        let d = TmpDir::new("skip");
        // A stray package.json inside node_modules / .git must NOT trigger node.
        d.touch("node_modules/leftpad/package.json");
        d.touch(".git/package.json");
        d.touch("target/package.json");
        assert!(detect_prologues(d.path()).is_empty());
    }

    #[test]
    fn does_not_descend_two_levels() {
        let d = TmpDir::new("deep");
        // Two levels down — not scanned.
        d.touch("a/b/Cargo.toml");
        assert!(detect_prologues(d.path()).is_empty());
    }

    #[test]
    fn reports_triggering_marker() {
        let d = TmpDir::new("marker");
        d.touch("Cargo.toml");
        assert_eq!(detect_with_markers(d.path()), vec![("rust", "Cargo.toml")]);
    }
}
