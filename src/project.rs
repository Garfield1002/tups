//! Project identity: where a project's state lives, and name validation.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::exit;

/// `~/.tup/<project>` — the per-project state dir (root/, services, net, init.pid, …).
pub(crate) fn project_dir(project: &str) -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".tup").join(project)
}

/// Validate a project name and ensure its rw root exists, returning the root path.
///
/// The charset is deliberately tight (`[A-Za-z0-9._-]`, no leading dot): the name is
/// interpolated into remote shell strings by `tup move` (ssh/pgrep), so anything looser
/// is both a portability hazard and a shell-injection vector. See DESIGN.md review notes.
pub(crate) fn ensure_project_root(project: &str) -> PathBuf {
    if !valid_name(project) {
        eprintln!(
            "tup: invalid project name '{project}' (allowed: letters, digits, '.', '_', '-'; not '.'/'..'; no leading '.')"
        );
        exit(2);
    }
    let root = project_dir(project).join("root");
    if let Err(e) = fs::create_dir_all(&root) {
        eprintln!("tup: cannot create {}: {e}", root.display());
        exit(1);
    }
    root
}

fn valid_name(project: &str) -> bool {
    !project.is_empty()
        && project != "."
        && project != ".."
        && !project.starts_with('.')
        && project
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::valid_name;

    #[test]
    fn accepts_ordinary_names() {
        for n in ["web", "my-app", "api_v2", "a.b.c", "Proj1", "x"] {
            assert!(valid_name(n), "{n} should be valid");
        }
    }

    #[test]
    fn rejects_empty_and_dot_forms() {
        for n in ["", ".", "..", ".hidden"] {
            assert!(!valid_name(n), "{n:?} should be rejected");
        }
    }

    #[test]
    fn rejects_shell_injection_and_path_chars() {
        // The name is interpolated into remote ssh/pgrep strings by `tup move`, so anything
        // that could break out of a shell token or traverse the filesystem must be rejected.
        for n in [
            "a/b", "a b", "a;rm", "a$(x)", "a`x`", "a|b", "a&b", "a>b", "a..b/..", "a\nb", "a'b",
            "a\"b", "a*b",
        ] {
            assert!(!valid_name(n), "{n:?} should be rejected");
        }
    }
}
