//! Nix environment integration — hermetic (roadmap slice 7, piece 1).
//!
//! The project's environment *is* a flake. `tup run --env nix <project>` evaluates the
//! project's flake to a single profile store path (a `buildEnv`) and makes that profile the
//! **only** source of userland: the host `/usr` is dropped entirely, and PATH is just
//! `<profile>/bin`. The closure already lives in the shared, read-only `/nix/store` bind, so
//! realising the env copies nothing into the container — it only adds a few symlinks and a
//! synthesized `/etc`.
//!
//! This is the hermetic variant on purpose: nothing from the host leaks in except the kernel,
//! `/nix/store`, `/dev`, and the project's `/work`. Shell, coreutils, git, CA certs — all come
//! from the flake.

use crate::project::project_dir;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{exit, Command};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvMode {
    /// Inherit the host's `/usr` + `/etc` (the original behaviour).
    Host,
    /// Hermetic: the environment is the project's flake; host `/usr` is dropped.
    Nix,
}

impl EnvMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            EnvMode::Host => "host",
            EnvMode::Nix => "nix",
        }
    }
}

pub(crate) fn env_mode_from_str(s: Option<&str>) -> EnvMode {
    match s {
        Some("nix") => EnvMode::Nix,
        Some("host") => EnvMode::Host,
        other => {
            eprintln!(
                "tup: unknown --env {:?} (want host|nix)",
                other.unwrap_or("")
            );
            exit(2);
        }
    }
}

/// The flake output `tup` builds, and the profile symlink / flake / etc layout under the
/// project state dir (`~/.tup/<project>/env/`).
const ENV_OUTPUT: &str = "tupEnv";

/// The default project environment: a hermetic base prologue (shell, coreutils, CA certs, and
/// the tools an agent needs to move around a repo). Written only when the project has no flake.
const DEFAULT_FLAKE: &str = r#"{
  description = "tup project environment";
  inputs.nixpkgs.url = "nixpkgs";
  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      # tup builds this output and binds it as the container's entire userland.
      packages.${system}.tupEnv = pkgs.buildEnv {
        name = "tup-env";
        paths = with pkgs; [ bashInteractive coreutils cacert gitMinimal ripgrep curl ];
      };
    };
}
"#;

pub(crate) fn env_dir(project: &str) -> PathBuf {
    project_dir(project).join("env")
}

/// The synthesized `/etc` for the hermetic root (passwd/group/resolv.conf live here).
pub(crate) fn etc_dir(project: &str) -> PathBuf {
    env_dir(project).join("etc")
}

/// `<profile>/etc/ssl/certs/ca-bundle.crt` — the CA bundle from the flake's `cacert`.
pub(crate) fn ca_bundle(profile: &std::path::Path) -> PathBuf {
    profile.join("etc/ssl/certs/ca-bundle.crt")
}

/// Host side: ensure the project's flake + synthesized `/etc` exist, build the flake, and
/// return the realised profile store path. Exits with a message on any failure.
pub(crate) fn realize(project: &str) -> PathBuf {
    let dir = env_dir(project);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("tup: create {}: {e}", dir.display());
        exit(1);
    }
    let flake = dir.join("flake.nix");
    if !flake.exists() {
        if let Err(e) = std::fs::write(&flake, DEFAULT_FLAKE) {
            eprintln!("tup: write {}: {e}", flake.display());
            exit(1);
        }
        eprintln!("[tup] wrote default env flake: {}", flake.display());
    }
    ensure_etc(project);

    let link = dir.join("profile");
    eprintln!("[tup] realising env: nix build .#{ENV_OUTPUT} …");
    let status = Command::new("nix")
        .args([
            "build",
            "--extra-experimental-features",
            "nix-command flakes",
            "--out-link",
        ])
        .arg(&link)
        .arg(format!("{}#{ENV_OUTPUT}", dir.display()))
        .status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            eprintln!("tup: nix build failed (exit {})", s.code().unwrap_or(-1));
            exit(1);
        }
        Err(e) => {
            eprintln!("tup: could not run `nix` ({e}). --env nix needs nix on PATH.");
            exit(1);
        }
    }
    match std::fs::canonicalize(&link) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("tup: resolve profile {}: {e}", link.display());
            exit(1);
        }
    }
}

/// Synthesize a minimal `/etc` for the hermetic root: an account for the mapped root uid and a
/// resolver. For `Net::Pasta` the sandbox binds its own resolv.conf over this one.
pub(crate) fn ensure_etc(project: &str) {
    let etc = etc_dir(project);
    if let Err(e) = std::fs::create_dir_all(&etc) {
        eprintln!("tup: create {}: {e}", etc.display());
        exit(1);
    }
    let _ = std::fs::write(
        etc.join("passwd"),
        "root:x:0:0:root:/work:/bin/sh\nnobody:x:65534:65534:nobody:/:/bin/sh\n",
    );
    let _ = std::fs::write(etc.join("group"), "root:x:0:\nnogroup:x:65534:\n");
    // Mirror the host resolver (useful for --net host); fall back to a public resolver.
    let resolv = std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default();
    let resolv = if resolv.trim().is_empty() {
        "nameserver 1.1.1.1\n".to_string()
    } else {
        resolv
    };
    let _ = std::fs::write(etc.join("resolv.conf"), resolv);
}

/// The default inner command for an env mode when the user gave none.
pub(crate) fn default_shell(env: EnvMode) -> Vec<OsString> {
    match env {
        // Resolved via PATH = <profile>/bin inside the hermetic root.
        EnvMode::Nix => vec![OsString::from("bash")],
        EnvMode::Host => vec![OsString::from("/bin/bash")],
    }
}
