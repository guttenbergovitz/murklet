//! SSRF defences: deny-by-default host allowlisting, IP range classification,
//! and strict remote URL validation.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

use crate::origin::error::OriginError;

/// Explicit allowlist of permitted remote origin hosts.
///
/// Empty allowlists deny every host by default so arbitrary public URL fetching
/// cannot be enabled accidentally.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostAllowlist {
    patterns: Vec<HostPattern>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostPattern {
    Exact(String),
    /// Matches any subdomain strictly beneath `suffix` (stored without leading `*.`).
    SubdomainOf(String),
}

impl HostAllowlist {
    /// Creates an empty allowlist that denies all remote hosts.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            patterns: Vec::new(),
        }
    }

    /// Builds an allowlist from exact hosts (for example `"cdn.example.com"`) or
    /// wildcard subdomain patterns (for example `"*.example.com"`).
    ///
    /// Patterns are normalised to lowercase ASCII with trailing FQDN dots stripped;
    /// blank or malformed entries are ignored.
    #[must_use]
    pub fn new<I, S>(entries: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut patterns = Vec::new();
        for entry in entries {
            let trimmed = entry.as_ref().trim().trim_end_matches('.');
            if trimmed.is_empty() {
                continue;
            }
            if let Some(suffix) = trimmed.strip_prefix("*.") {
                let normalised = suffix.to_ascii_lowercase();
                if !normalised.is_empty() && !normalised.contains('*') {
                    patterns.push(HostPattern::SubdomainOf(normalised));
                }
            } else if !trimmed.contains('*') {
                patterns.push(HostPattern::Exact(trimmed.to_ascii_lowercase()));
            }
        }
        Self { patterns }
    }

    /// Returns `true` when no hosts are permitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.patterns.is_empty()
    }

    /// Reports whether `host` matches at least one configured allowlist pattern.
    #[must_use]
    pub fn is_allowed(&self, host: &str) -> bool {
        let normalised = host.trim().trim_end_matches('.').to_ascii_lowercase();
        if normalised.is_empty() {
            return false;
        }

        self.patterns.iter().any(|pattern| match pattern {
            HostPattern::Exact(exact) => normalised == *exact,
            HostPattern::SubdomainOf(suffix) => {
                // Require an explicit dot boundary so `evilexample.com` never matches `*.example.com`.
                normalised.len() > suffix.len()
                    && normalised.ends_with(suffix.as_str())
                    && normalised
                        .as_bytes()
                        .get(normalised.len() - suffix.len() - 1)
                        == Some(&b'.')
            }
        })
    }
}

/// Validates a remote HTTP(S) URL against scheme, credential, allowlist, and IP literal rules.
///
/// # Errors
///
/// Returns:
/// - [`OriginError::InvalidPath`] for malformed URLs, non-`http`/`https` schemes, or embedded credentials.
/// - [`OriginError::HostNotAllowed`] when the host is not permitted by `allowlist`.
/// - [`OriginError::ForbiddenIpAddress`] when the host is a forbidden IP literal and `enforce_ip_block` is `true`.
pub fn validate_remote_url(
    raw_url: &str,
    allowlist: &HostAllowlist,
    enforce_ip_block: bool,
) -> Result<Url, OriginError> {
    // Reject backslashes and ASCII control characters before WHATWG normalisation
    // to prevent URL-parser differential bypasses (e.g. `host\@127.0.0.1`).
    if raw_url.contains('\\') || raw_url.bytes().any(|b| b.is_ascii_control()) {
        return Err(OriginError::InvalidPath(
            "remote URL contains disallowed backslash or control characters".to_owned(),
        ));
    }

    let parsed = Url::parse(raw_url)
        .map_err(|_| OriginError::InvalidPath("invalid remote URL".to_owned()))?;

    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(OriginError::InvalidPath(
            "remote URL must use http or https scheme".to_owned(),
        ));
    }

    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(OriginError::InvalidPath(
            "remote URL must not contain embedded credentials".to_owned(),
        ));
    }

    let Some(host) = parsed.host() else {
        return Err(OriginError::InvalidPath(
            "remote URL must contain a host".to_owned(),
        ));
    };

    if enforce_ip_block {
        match host {
            Host::Ipv4(ip) if is_forbidden_ipv4(ip) => {
                return Err(OriginError::ForbiddenIpAddress);
            }
            Host::Ipv6(ip) if is_forbidden_ipv6(ip) => {
                return Err(OriginError::ForbiddenIpAddress);
            }
            _ => {}
        }
    }

    let host_str = parsed
        .host_str()
        .ok_or_else(|| OriginError::InvalidPath("remote URL must contain a host".to_owned()))?;

    // Strip brackets around IPv6 literals before matching against allowlist entries.
    let unbracketed = host_str
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host_str);

    if !allowlist.is_allowed(unbracketed) {
        return Err(OriginError::HostNotAllowed);
    }

    Ok(parsed)
}

/// Reports whether `ip` belongs to a non-public, internal, metadata, or reserved range.
#[must_use]
pub const fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_ipv4(v4),
        IpAddr::V6(v6) => is_forbidden_ipv6(v6),
    }
}

/// Classifies IPv4 addresses that must never be reached by an image proxy.
#[must_use]
pub const fn is_forbidden_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _d] = ip.octets();

    // 0.0.0.0/8 ("this network" / unspecified)
    if a == 0 {
        return true;
    }
    // 10.0.0.0/8 (RFC 1918 private)
    if a == 10 {
        return true;
    }
    // 100.64.0.0/10 (RFC 6598 CGNAT / shared address space)
    if a == 100 && (b & 0b1100_0000) == 64 {
        return true;
    }
    // 127.0.0.0/8 (loopback)
    if a == 127 {
        return true;
    }
    // 169.254.0.0/16 (link-local and cloud instance metadata)
    if a == 169 && b == 254 {
        return true;
    }
    // 172.16.0.0/12 (RFC 1918 private)
    if a == 172 && (b & 0b1111_0000) == 16 {
        return true;
    }
    // 192.0.0.0/24 (IETF protocol assignments) & 192.0.2.0/24 (TEST-NET-1)
    if a == 192 && b == 0 && (c == 0 || c == 2) {
        return true;
    }
    // 192.168.0.0/16 (RFC 1918 private)
    if a == 192 && b == 168 {
        return true;
    }
    // 198.18.0.0/15 (benchmarking)
    if a == 198 && (b == 18 || b == 19) {
        return true;
    }
    // 198.51.100.0/24 (TEST-NET-2)
    if a == 198 && b == 51 && c == 100 {
        return true;
    }
    // 203.0.113.0/24 (TEST-NET-3)
    if a == 203 && b == 0 && c == 113 {
        return true;
    }
    // 224.0.0.0/4 (multicast) and 240.0.0.0/4 (reserved + broadcast 255.255.255.255)
    a >= 224
}

/// Classifies IPv6 addresses that must never be reached by an image proxy,
/// including IPv4-mapped (`::ffff:0:0/96`) and NAT64 prefixes.
#[must_use]
pub const fn is_forbidden_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(mapped_v4) = ip.to_ipv4_mapped() {
        return is_forbidden_ipv4(mapped_v4);
    }

    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return true;
    }

    let segments = ip.segments();
    // fc00::/7 (unique local IPv6)
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    // fe80::/10 (link-local)
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // 100::/64 (RFC 6666 discard-only)
    if segments[0] == 0x0100 && segments[1] == 0 && segments[2] == 0 && segments[3] == 0 {
        return true;
    }
    // 2001:db8::/32 (documentation)
    if segments[0] == 0x2001 && segments[1] == 0x0db8 {
        return true;
    }
    // 64:ff9b::/96 and 64:ff9b:1::/48 (NAT64 translation prefixes)
    if segments[0] == 0x0064 && segments[1] == 0xff9b {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allowlist_is_deny_by_default_and_handles_exact_and_wildcard_hosts() {
        assert!(!HostAllowlist::empty().is_allowed("example.com"));

        let list = HostAllowlist::new(["CDN.Example.COM", "*.assets.example.org."]);
        assert!(list.is_allowed("cdn.example.com"));
        assert!(list.is_allowed("cdn.example.com."));
        assert!(!list.is_allowed("example.com"));

        assert!(list.is_allowed("img.assets.example.org"));
        assert!(list.is_allowed("a.b.assets.example.org"));
        assert!(!list.is_allowed("assets.example.org"));
        assert!(!list.is_allowed("evilassets.example.org"));
    }

    #[test]
    fn forbidden_ip_blocks_private_loopback_metadata_cgnat_and_mapped_ipv6() {
        let forbidden: [&str; 17] = [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "100.127.255.254",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ];

        for addr in forbidden {
            let ip: IpAddr = addr.parse().unwrap();
            assert!(is_forbidden_ip(ip), "expected {addr} to be forbidden");
        }

        let allowed: [&str; 4] = [
            "93.184.216.34",
            "1.1.1.1",
            "2606:2800:220:1:248:1893:25c8:1946",
            "::ffff:93.184.216.34",
        ];

        for addr in allowed {
            let ip: IpAddr = addr.parse().unwrap();
            assert!(!is_forbidden_ip(ip), "expected {addr} to be allowed");
        }
    }

    #[test]
    fn validate_remote_url_enforces_scheme_credentials_allowlist_and_ip_literals() {
        let allowlist = HostAllowlist::new(["cdn.example.com", "127.0.0.1", "::ffff:127.0.0.1"]);

        assert!(validate_remote_url("https://cdn.example.com/a.jpg", &allowlist, true).is_ok());

        assert!(matches!(
            validate_remote_url("file:///etc/passwd", &allowlist, true),
            Err(OriginError::InvalidPath(_))
        ));
        assert!(matches!(
            validate_remote_url("https://user:pass@cdn.example.com/a.jpg", &allowlist, true),
            Err(OriginError::InvalidPath(_))
        ));
        assert_eq!(
            validate_remote_url("https://other.example.com/a.jpg", &allowlist, true),
            Err(OriginError::HostNotAllowed)
        );
        // Even if an operator misconfigures a private IP in the allowlist, IP blocking still wins.
        assert_eq!(
            validate_remote_url("http://127.0.0.1/secret", &allowlist, true),
            Err(OriginError::ForbiddenIpAddress)
        );
        assert_eq!(
            validate_remote_url("http://[::ffff:127.0.0.1]/secret", &allowlist, true),
            Err(OriginError::ForbiddenIpAddress)
        );
    }
}
