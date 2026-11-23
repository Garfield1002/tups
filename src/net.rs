//! Network mode for a project: the netns choice and the DNS relay that goes with pasta.

use crate::project::project_dir;
use std::fs;
use std::path::PathBuf;
use std::process::exit;

/// In-namespace address the guest sends DNS to; pasta intercepts and forwards it to the
/// host resolver. Must be non-loopback (pasta rejects loopback forward addresses).
pub(crate) const PASTA_DNS: &str = "169.254.0.53";

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Net {
    Pasta,
    Host,
    None,
}

impl Net {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Net::Pasta => "pasta",
            Net::Host => "host",
            Net::None => "none",
        }
    }
}

pub(crate) fn net_from_str(v: Option<&str>) -> Net {
    match v {
        Some("pasta") => Net::Pasta,
        Some("host") => Net::Host,
        Some("none") => Net::None,
        other => {
            eprintln!("tup: unknown --net '{}'", other.unwrap_or(""));
            exit(2);
        }
    }
}

/// Persist/read a project's net mode, so `tup move` restores it instead of silently
/// resetting to the pasta default (which would hand a `--net none` session open egress).
pub(crate) fn write_net(project: &str, net: Net) {
    let _ = fs::write(project_dir(project).join("net"), net.label());
}

pub(crate) fn read_net_label(project: &str) -> String {
    fs::read_to_string(project_dir(project).join("net"))
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "pasta".to_string())
}

/// Write the per-project resolv.conf pointing at the pasta DNS relay; returns its path.
pub(crate) fn ensure_resolv(project: &str) -> PathBuf {
    let dir = project_dir(project);
    let _ = fs::create_dir_all(&dir);
    let p = dir.join("resolv.conf");
    let _ = fs::write(&p, format!("nameserver {PASTA_DNS}\n"));
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_modes() {
        assert!(net_from_str(Some("pasta")) == Net::Pasta);
        assert!(net_from_str(Some("host")) == Net::Host);
        assert!(net_from_str(Some("none")) == Net::None);
    }

    #[test]
    fn labels_round_trip() {
        for m in [Net::Pasta, Net::Host, Net::None] {
            assert!(net_from_str(Some(m.label())) == m);
        }
    }

    #[test]
    fn pasta_dns_is_non_loopback() {
        // pasta rejects a loopback forward address, so this must never drift into 127.0.0.0/8.
        assert!(!PASTA_DNS.starts_with("127."));
    }
}
