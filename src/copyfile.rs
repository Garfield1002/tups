//! `tup cp` — copy files between the host and a project's working tree (like `docker cp`).
//!
//! One side of the copy addresses a project with a `<project>:<path>` spec; the other is an
//! ordinary host path. The `<project>:` side resolves against the project's *writable* root
//! (`~/.tup/<project>/root`, mounted as `/work`): a relative path is taken relative to `/work`,
//! an absolute path must be under `/work` (the only real/writable tree), and any `..` that would
//! climb out of the root is refused. The path-mapping/escape logic is the pure
//! [`resolve_in_root`] so it can be unit-tested without touching the filesystem.

use crate::project::{ensure_project_root, valid_name};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::exit;

/// Split a cp argument into its `(project, path)` parts if it is a `<project>:<path>` spec.
///
/// It is a project spec only when a `:` is present and the text before the first `:` is a valid
/// project name. Because [`valid_name`] rejects `/` and leading `.`/`-`, ordinary host paths
/// (`/abs/p`, `./rel`, `../up`) never match, so there is no ambiguity with real paths.
fn split_project_spec(s: &str) -> Option<(String, String)> {
    let (maybe_proj, path) = s.split_once(':')?;
    if valid_name(maybe_proj) {
        Some((maybe_proj.to_string(), path.to_string()))
    } else {
        None
    }
}

/// Map the container side of a `tup cp` to a concrete host path under `root`
/// (`~/.tup/<project>/root`, the container's `/work`). Pure: it does not touch the filesystem,
/// so the mapping and the `..`-escape guard are unit-testable.
///
/// `path` is the portion after `proj:`. A relative path is taken relative to `/work`; an
/// absolute path must be `/work` or under `/work/`. Returns an error for a container path
/// outside `/work`, or for any path that would escape `root` via `..`.
pub(crate) fn resolve_in_root(root: &Path, path: &str) -> Result<PathBuf, String> {
    // Map the container-visible path to one relative to the root.
    let rel: &str = if path.starts_with('/') {
        if path == "/work" {
            ""
        } else if let Some(r) = path.strip_prefix("/work/") {
            r
        } else {
            return Err(format!(
                "container path {path:?} is outside /work; only /work is writable/real in a tup container"
            ));
        }
    } else {
        path
    };

    // Lexically resolve the relative path against the root, forbidding any climb above it. We
    // can't canonicalize (the path may not exist yet on a copy-in), so we walk components and
    // keep a depth counter instead of trusting the filesystem.
    let mut out = root.to_path_buf();
    let mut depth: usize = 0;
    for comp in Path::new(rel).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if depth == 0 {
                    return Err(format!(
                        "container path {path:?} escapes the project root via '..'"
                    ));
                }
                depth -= 1;
                out.pop();
            }
            Component::Normal(c) => {
                depth += 1;
                out.push(c);
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!("invalid container path {path:?}"));
            }
        }
    }
    Ok(out)
}

/// If `dst` is an existing directory, copy *into* it (`dst/<basename of src>`), matching `cp`
/// and `docker cp`. Otherwise `dst` is the destination path itself.
fn final_dest(src: &Path, dst: &Path) -> PathBuf {
    if dst.is_dir() {
        if let Some(name) = src.file_name() {
            return dst.join(name);
        }
    }
    dst.to_path_buf()
}

/// Recursively copy a file or directory tree. Creates parent directories as needed.
fn copy_recursive(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        // fs::copy follows a symlink and copies its target's bytes — fine for this tool.
        fs::copy(src, dst)?;
        Ok(())
    }
}

pub(crate) fn cmd_cp(args: &[String]) {
    if args.len() != 2 {
        eprintln!(
            "usage: tup cp <src> <dst>   (exactly one side is <project>:<path>)\n  \
             tup cp ./file.txt proj:sub/        host → container (relative to /work)\n  \
             tup cp proj:sub/file.txt ./out/    container → host\n  \
             a <project>: path maps under ~/.tup/<project>/root (the container's /work)"
        );
        exit(2);
    }
    let src = &args[0];
    let dst = &args[1];
    match (split_project_spec(src), split_project_spec(dst)) {
        (Some(_), Some(_)) => {
            eprintln!("tup: cp between two containers is not supported (one side must be a host path)");
            exit(2);
        }
        (None, None) => {
            eprintln!("tup: one side must be <project>:<path> (host→host is just `cp`)");
            exit(2);
        }
        // container → host
        (Some((project, cpath)), None) => {
            let root = ensure_project_root(&project);
            let resolved = resolve_in_root(&root, &cpath).unwrap_or_else(|e| {
                eprintln!("tup: {e}");
                exit(2);
            });
            if !resolved.exists() {
                eprintln!(
                    "tup: {project}:{cpath} does not exist ({})",
                    resolved.display()
                );
                exit(1);
            }
            let dest = final_dest(&resolved, Path::new(dst));
            if let Err(e) = copy_recursive(&resolved, &dest) {
                eprintln!("tup: copy failed: {e}");
                exit(1);
            }
            println!("copied {project}:{cpath} -> {}", dest.display());
        }
        // host → container
        (None, Some((project, cpath))) => {
            let root = ensure_project_root(&project);
            let srcp = Path::new(src);
            if !srcp.exists() {
                eprintln!("tup: {src}: no such file or directory");
                exit(1);
            }
            let resolved = resolve_in_root(&root, &cpath).unwrap_or_else(|e| {
                eprintln!("tup: {e}");
                exit(2);
            });
            // "copy into an existing dir" semantics; the appended basename is a single Normal
            // component, so the result is still inside the root the escape check already proved.
            let dest = final_dest(srcp, &resolved);
            if let Err(e) = copy_recursive(srcp, &dest) {
                eprintln!("tup: copy failed: {e}");
                exit(1);
            }
            println!("copied {src} -> {project}:{}", cpath);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/home/u/.tup/web/root")
    }

    #[test]
    fn relative_maps_under_work() {
        let r = root();
        assert_eq!(resolve_in_root(&r, "foo").unwrap(), r.join("foo"));
        assert_eq!(resolve_in_root(&r, "a/b/c").unwrap(), r.join("a/b/c"));
    }

    #[test]
    fn absolute_work_paths_map_under_root() {
        let r = root();
        assert_eq!(resolve_in_root(&r, "/work/foo").unwrap(), r.join("foo"));
        assert_eq!(resolve_in_root(&r, "/work").unwrap(), r);
        assert_eq!(resolve_in_root(&r, "/work/a/b").unwrap(), r.join("a/b"));
    }

    #[test]
    fn absolute_outside_work_is_refused() {
        let r = root();
        for p in ["/etc/passwd", "/", "/workspace", "/work2", "/home/u"] {
            assert!(
                resolve_in_root(&r, p).is_err(),
                "{p:?} should be refused (outside /work)"
            );
        }
    }

    #[test]
    fn parent_dir_escape_is_refused() {
        let r = root();
        for p in ["..", "../etc", "a/../../etc", "/work/../etc", "/work/../../x"] {
            assert!(
                resolve_in_root(&r, p).is_err(),
                "{p:?} should be refused (escapes root)"
            );
        }
    }

    #[test]
    fn interior_parent_dir_is_allowed() {
        // `..` that stays within the root is fine.
        let r = root();
        assert_eq!(resolve_in_root(&r, "a/b/../c").unwrap(), r.join("a/c"));
        assert_eq!(resolve_in_root(&r, "a/./b").unwrap(), r.join("a/b"));
    }

    #[test]
    fn split_recognizes_project_specs_not_host_paths() {
        assert_eq!(
            split_project_spec("web:foo/bar"),
            Some(("web".to_string(), "foo/bar".to_string()))
        );
        assert_eq!(
            split_project_spec("my-app:/work/x"),
            Some(("my-app".to_string(), "/work/x".to_string()))
        );
        // Host paths and non-name prefixes are not specs.
        assert_eq!(split_project_spec("/abs/path"), None);
        assert_eq!(split_project_spec("./rel"), None);
        assert_eq!(split_project_spec("../up"), None);
        assert_eq!(split_project_spec("plainfile"), None);
        assert_eq!(split_project_spec("a/b:c"), None); // '/' before ':' → not a name
    }
}
