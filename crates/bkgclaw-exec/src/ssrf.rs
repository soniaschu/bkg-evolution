//! SSRF guard for outbound web tools.
//!
//! A model that has been fed a crafted prompt must not be able to reach the
//! machine's own services: localhost, private ranges, link-local metadata
//! endpoints. The guard checks the URL scheme and resolves the host before
//! any request is made, and reqwest's redirect policy re-checks every hop —
//! a public URL that redirects to `169.254.169.254` is a classic exfiltration
//! route and must fail, not follow.

use std::net::{IpAddr, ToSocketAddrs};

/// Whether a URL may be fetched. Returns a human-readable refusal reason.
/// `Ok(())` means the scheme is web and the host resolves to public space.
pub fn url_allowed(raw: &str) -> Result<(), String> {
    let url = parse_url(raw)?;
    check_parsed(&url)
}

/// Minimal URL parts this guard needs. Kept hand-rolled so the guard is
/// testable without a URL crate and without network I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UrlParts {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

pub fn parse_url(raw: &str) -> Result<UrlParts, String> {
    let (scheme, rest) = raw
        .split_once("://")
        .ok_or_else(|| format!("`{raw}` has no scheme; only http and https are fetched"))?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(format!(
            "scheme `{scheme}` is not fetched; only http and https"
        ));
    }
    // Strip path, query and fragment.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    // Strip userinfo — `http://user:pass@host` hides the host.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => (h, p),
        // Bare IPv6 literal like [::1]:8080 — the brackets keep the split
        // honest; anything bracketed is parsed by resolution below.
        _ => (authority, ""),
    };
    let port: u16 = match port {
        "" => {
            if scheme == "https" {
                443
            } else {
                80
            }
        }
        text => text
            .parse()
            .map_err(|_| format!("`{raw}` has a malformed port"))?,
    };
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if host.is_empty() {
        return Err(format!("`{raw}` has no host"));
    }
    Ok(UrlParts { scheme, host, port })
}

fn check_parsed(url: &UrlParts) -> Result<(), String> {
    // A host that is already an IP literal needs no DNS.
    if let Ok(ip) = url.host.parse::<IpAddr>() {
        if is_private_ip(&ip) {
            return Err(format!(
                "`{}` resolves to the private address {ip}; refusing",
                url.host
            ));
        }
        return Ok(());
    }
    // Resolve and refuse if ANY address is private: round-robin DNS with one
    // internal address is still an internal endpoint.
    let addrs: Vec<std::net::IpAddr> = (url.host.as_str(), url.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve `{}`: {e}", url.host))?
        .map(|a| a.ip())
        .collect();
    if addrs.is_empty() {
        return Err(format!("`{}` resolves to nothing", url.host));
    }
    for ip in &addrs {
        if is_private_ip(ip) {
            return Err(format!(
                "`{}` resolves to the private address {ip}; refusing",
                url.host
            ));
        }
    }
    Ok(())
}

/// Whether an IP points at this machine or its private network. The ranges
/// are the ones cloud metadata services and home routers live in.
pub fn is_private_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 0
                || octets[0] == 10
                || octets[0] == 127
                || (octets[0] == 100 && (64..=127).contains(&octets[1])) // CGNAT
                || (octets[0] == 169 && octets[1] == 254) // link-local + metadata
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 168)
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            segments[0] == 0 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0
                && segments[4] == 0 && segments[5] == 0 && segments[6] == 0
                && (segments[7] == 0 || segments[7] == 1) // ::, ::1
                || (segments[0] & 0xfe00) == 0xfc00 // unique local fc00::/7
                || (segments[0] & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_and_https_are_fetchable_schemes() {
        assert!(parse_url("http://example.com").is_ok());
        assert!(parse_url("https://example.com").is_ok());
        assert!(parse_url("file:///etc/passwd").is_err());
        assert!(parse_url("gopher://example.com").is_err());
        assert!(
            parse_url("example.com").is_err(),
            "no scheme is not fetchable"
        );
    }

    #[test]
    fn userinfo_cannot_hide_the_host() {
        let parts = parse_url("http://user:pass@127.0.0.1:8080/x").unwrap();
        assert_eq!(parts.host, "127.0.0.1");
        assert_eq!(parts.port, 8080);
    }

    #[test]
    fn private_ranges_are_refused_without_dns() {
        for ip in [
            "127.0.0.1",
            "10.0.0.5",
            "172.16.1.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
        ] {
            let url = format!("http://{ip}/");
            let err = url_allowed(&url).unwrap_err();
            assert!(err.contains("private"), "{ip}: {err}");
        }
    }

    #[test]
    fn public_v4_and_v6_literals_are_allowed() {
        assert!(url_allowed("http://93.184.216.34/").is_ok());
        assert!(url_allowed("https://[2606:2800:220:1:248:1893:25c8:1946]/").is_ok());
    }

    #[test]
    fn the_loopback_v6_literal_is_private() {
        assert!(url_allowed("http://[::1]:8080/").is_err());
        assert!(url_allowed("http://[fe80::1]/").is_err());
    }

    #[test]
    fn a_dns_name_that_resolves_privately_is_refused() {
        // `localhost` resolves through /etc/hosts to 127.0.0.1 on every
        // machine this test can run on.
        let err = url_allowed("http://localhost:3000/").unwrap_err();
        assert!(err.contains("private"), "{err}");
    }

    #[test]
    fn the_nim_gateway_is_public_and_allowed() {
        // Real resolution: the operator's gateway must pass the guard, or
        // the agent could never reach its own model endpoint. Offline, DNS
        // fails and the test skips rather than lies.
        match url_allowed("https://nim.eysho.info/v1/models") {
            Ok(()) => {}
            Err(e) if e.contains("resolve") => {}
            Err(e) => panic!("the public gateway must not be refused as private: {e}"),
        }
    }

    #[test]
    fn ports_default_per_scheme() {
        assert_eq!(parse_url("http://example.com").unwrap().port, 80);
        assert_eq!(parse_url("https://example.com").unwrap().port, 443);
        assert!(parse_url("http://example.com:not-a-port").is_err());
    }
}
