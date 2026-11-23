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

/// Exit with a usage error unless `project` is a valid name. Every verb that turns a name
/// into a filesystem path (`~/.tup/<name>`) or a remote shell string MUST call this first —
/// an unvalidated name like `..` resolves `project_dir` to the parent of `~/.tup`, which a
/// destructive verb (`tup rm`) would then delete. See [`valid_name`].
pub(crate) fn ensure_valid_name(project: &str) {
    if !valid_name(project) {
        eprintln!(
            "tup: invalid project name '{project}' (allowed: letters, digits, '.', '_', '-'; not '.'/'..'; no leading '.' or '-')"
        );
        exit(2);
    }
}

/// Validate a project name and ensure its rw root exists, returning the root path.
pub(crate) fn ensure_project_root(project: &str) -> PathBuf {
    ensure_valid_name(project);
    let root = project_dir(project).join("root");
    if let Err(e) = fs::create_dir_all(&root) {
        eprintln!("tup: cannot create {}: {e}", root.display());
        exit(1);
    }
    root
}

/// The charset is deliberately tight (`[A-Za-z0-9._-]`, no leading dot or dash): the name is
/// interpolated into remote shell strings by `tup move` (ssh/pgrep) and into `~/.tup/<name>`
/// paths, so anything looser is a shell-injection, path-traversal, and flag-confusion vector
/// (a leading `-` would also masquerade as a CLI flag). See DESIGN.md review notes.
pub(crate) fn valid_name(project: &str) -> bool {
    !project.is_empty()
        && project != "."
        && project != ".."
        && !project.starts_with('.')
        && !project.starts_with('-')
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
    fn rejects_leading_dash_flag_lookalikes() {
        // A leading '-' would both masquerade as a CLI flag and (for `..`-style names) was the
        // path-traversal vector the lifecycle verbs now guard against.
        for n in ["-f", "-y", "--force", "--yes", "-"] {
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
