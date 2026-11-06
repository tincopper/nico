//! What a destination host may be: a name, never a scheme, a port or an IPv6 literal.
//!
//! A host reaches the wire twice — as the `Host` header and as the address a peer resolves — so a
//! value carrying a scheme, a port or whitespace would either address nothing or split one request
//! into two. The rule is one rule, shared **across crates**: the document model applies it to
//! `routes[].host` and `upstreams[].host`, and an adapter that derives a destination from a provider
//! `base_url` applies it to the host it parsed. Two copies would eventually disagree about, say,
//! whether a trailing colon counts.
//!
//! The rule is about the *host*, not the URL: which URL forms are addressable belongs to the crate
//! that interprets a base URL. A colon-bearing host is refused here because this build's destinations
//! carry a bare name and the data plane writes `host:port` without re-bracketing, so an IPv6 literal
//! would become a `Host` value nothing can read.

use std::borrow::Cow;

/// Upper bound for a host, matching the DNS name limit.
pub const MAX_HOST_BYTES: usize = 253;

/// Why a value is not a destination this build can address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HostError {
    /// The host names nothing at all.
    Empty,
    /// The host is longer than the DNS name limit.
    TooLong,
    /// The host carries a `://`, so it is a URL rather than a name.
    Scheme,
    /// The host carries a `:`: a port, or an IPv6 literal.
    Port,
    /// The host carries whitespace.
    Whitespace,
}

impl HostError {
    /// Returns the rule the host broke, for the caller to report under its own field name.
    ///
    /// This is the only spelling of the rule: a caller names its own field and this text, so two
    /// layers cannot describe the same refusal differently. The upper-bound rule formats
    /// [`MAX_HOST_BYTES`] instead of quoting a literal, so the message and the check cannot disagree.
    #[must_use]
    pub fn reason(&self) -> Cow<'static, str> {
        match self {
            Self::Empty => Cow::Borrowed("must not be empty"),
            Self::TooLong => Cow::Owned(format!("must be at most {MAX_HOST_BYTES} bytes")),
            Self::Scheme => Cow::Borrowed("must not include a scheme"),
            Self::Port => Cow::Borrowed("must not include a port or an IPv6 literal"),
            Self::Whitespace => Cow::Borrowed("must not contain whitespace"),
        }
    }
}

/// Checks that `host` is a destination this build can address.
///
/// The checks run in one fixed order — empty, oversized, scheme, port, whitespace — so a value that
/// breaks several rules always reports the same one, whichever caller asks.
///
/// # Errors
/// Returns the first [`HostError`] the value breaks.
///
/// # Examples
///
/// ```
/// use nico_core::{HostError, validate_host};
///
/// assert!(validate_host("api.example").is_ok());
/// assert!(validate_host("127.0.0.1").is_ok());
/// assert_eq!(validate_host("api.example:8080"), Err(HostError::Port));
/// assert_eq!(validate_host("[::1]"), Err(HostError::Port));
/// ```
pub fn validate_host(host: &str) -> Result<(), HostError> {
    if host.is_empty() {
        return Err(HostError::Empty);
    }
    if host.len() > MAX_HOST_BYTES {
        return Err(HostError::TooLong);
    }
    if host.contains("://") {
        return Err(HostError::Scheme);
    }
    if host.contains(':') {
        return Err(HostError::Port);
    }
    if host.chars().any(char::is_whitespace) {
        return Err(HostError::Whitespace);
    }
    Ok(())
}

/// Reports whether two hosts name the same destination.
///
/// An HTTP authority is case-insensitive: `API.example` and `api.example` reach the same server, so a
/// document that lists both has listed one route twice. The document model refuses such a duplicate
/// and the router chooses a route with the same comparison, which is why the rule lives here beside
/// [`validate_host`] — this module already owns "one rule, shared across crates", and two spellings of
/// *this* one is how a validator ends up promising a refusal the router does not agree with.
#[must_use]
pub fn hosts_match(left: &str, right: &str) -> bool {
    left.eq_ignore_ascii_case(right)
}

/// Returns the key a host is deduplicated under.
///
/// [`hosts_match`] is the comparison; a set needs a value. ASCII-lowercasing is that comparison's
/// canonical form, and a test pins the two together: `hosts_match(a, b)` must equal
/// `host_key(a) == host_key(b)` for every pair, or a document would be refused as a duplicate the
/// router routes as one route, or accepted as distinct and routed as another.
#[must_use]
pub fn host_key(host: &str) -> String {
    host.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::{HostError, MAX_HOST_BYTES, host_key, hosts_match, validate_host};

    #[test]
    fn a_name_is_a_host() {
        for host in ["api.example", "api.example.", "127.0.0.1", "a"] {
            assert_eq!(validate_host(host), Ok(()), "{host} is a name");
        }
    }

    #[test]
    fn a_url_is_not_a_host() {
        assert_eq!(validate_host("https://api.example"), Err(HostError::Scheme));
    }

    #[test]
    fn a_port_or_an_ipv6_literal_is_not_a_host() {
        for host in ["api.example:8080", "[::1]", "::1", "api.example:"] {
            assert_eq!(validate_host(host), Err(HostError::Port), "{host}");
        }
    }

    #[test]
    fn whitespace_is_not_a_host() {
        assert_eq!(validate_host("api example"), Err(HostError::Whitespace));
    }

    #[test]
    fn an_empty_value_names_nothing() {
        assert_eq!(validate_host(""), Err(HostError::Empty));
    }

    #[test]
    fn the_boundary_is_the_dns_name_limit() {
        let at_bound = "a".repeat(MAX_HOST_BYTES);
        assert_eq!(validate_host(&at_bound), Ok(()));
        let over_bound = "a".repeat(MAX_HOST_BYTES + 1);
        assert_eq!(validate_host(&over_bound), Err(HostError::TooLong));
    }

    #[test]
    fn the_comparison_and_the_key_are_the_same_rule() {
        let cases = [
            ("api.example", "api.example"),
            ("API.example", "api.example"),
            ("Api.Example.", "api.example."),
            ("api.example", "api.example."),
            ("127.0.0.1", "127.0.0.1"),
        ];
        for (left, right) in cases {
            assert_eq!(
                hosts_match(left, right),
                host_key(left) == host_key(right),
                "`{left}` vs `{right}`: the comparison and the key must agree"
            );
        }
        assert!(hosts_match("API.example", "api.example"));
        assert!(
            !hosts_match("api.example", "api.example."),
            "a trailing dot is a different name"
        );
    }

    #[test]
    fn the_reason_quotes_the_bound_it_applied() {
        assert!(
            HostError::TooLong
                .reason()
                .contains(&MAX_HOST_BYTES.to_string()),
            "the message names the limit the check used"
        );
    }
}
