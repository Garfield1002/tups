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

/// Transport for a forwarded port.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Proto {
    Tcp,
    Udp,
}

/// A port forward (pasta only): expose the container's `container` port on the host's `host`
/// port. `host` is pasta's "local" port, `container` the in-namespace (guest) port.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct PortForward {
    pub proto: Proto,
    pub host: u16,
    pub container: u16,
}

fn parse_port(s: &str) -> Result<u16, String> {
    let s = s.trim();
    match s.parse::<u16>() {
        Ok(0) => Err("port 0 is not allowed".to_string()),
        Ok(p) => Ok(p),
        Err(_) => Err(format!("invalid port {s:?}")),
    }
}

/// Parse `[tcp|udp:]HOST:CONTAINER` (or a single port, where host==container). Proto defaults
/// to tcp. Used by the `--port` CLI flag and, via a reconstructed spec, by `read_ports`.
pub(crate) fn port_from_str(s: &str) -> Result<PortForward, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty port spec".to_string());
    }
    // Optional `tcp:` / `udp:` proto prefix (case-insensitive); default tcp.
    let (proto, rest) = match s.split_once(':') {
        Some((head, tail)) if head.eq_ignore_ascii_case("tcp") => (Proto::Tcp, tail),
        Some((head, tail)) if head.eq_ignore_ascii_case("udp") => (Proto::Udp, tail),
        _ => (Proto::Tcp, s),
    };
    // Remaining is either a single PORT (host==container) or HOST:CONTAINER. A trailing colon
    // (`tcp:`), an empty side (`:3000`), or extra colons (`8080:3000:9`) are all rejected here.
    let (host, container) = match rest.split_once(':') {
        Some((h, c)) => (parse_port(h)?, parse_port(c)?),
        None => {
            let p = parse_port(rest)?;
            (p, p)
        }
    };
    Ok(PortForward {
        proto,
        host,
        container,
    })
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

    #[test]
    fn port_single_defaults_tcp_and_mirrors() {
        let p = port_from_str("3000").unwrap();
        assert_eq!(p, PortForward { proto: Proto::Tcp, host: 3000, container: 3000 });
    }

    #[test]
    fn port_host_container_pair() {
        let p = port_from_str("18080:3000").unwrap();
        assert_eq!(p, PortForward { proto: Proto::Tcp, host: 18080, container: 3000 });
    }

    #[test]
    fn port_explicit_proto() {
        assert_eq!(
            port_from_str("tcp:8080:3000").unwrap(),
            PortForward { proto: Proto::Tcp, host: 8080, container: 3000 }
        );
        assert_eq!(
            port_from_str("udp:53:5353").unwrap(),
            PortForward { proto: Proto::Udp, host: 53, container: 5353 }
        );
        // proto prefix with a single port mirrors.
        assert_eq!(
            port_from_str("udp:9999").unwrap(),
            PortForward { proto: Proto::Udp, host: 9999, container: 9999 }
        );
        // proto is case-insensitive.
        assert_eq!(port_from_str("UDP:53").unwrap().proto, Proto::Udp);
    }

    #[test]
    fn port_rejects_malformed() {
        for bad in [
            "",            // empty
            "tcp:",        // proto but no port
            ":3000",       // empty host
            "8080:",       // empty container
            "8080:3000:9", // too many parts
            "abc",         // not a number
            "70000",       // out of u16 range
            "0",           // port 0
            "8080:0",      // container port 0
            "-5",          // negative
        ] {
            assert!(port_from_str(bad).is_err(), "expected {bad:?} to be rejected");
        }
    }
}
