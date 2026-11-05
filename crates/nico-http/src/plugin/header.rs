//! The header grammar a plugin's own names and values must satisfy.
//!
//! A plugin hands the data plane strings, not framework values: the contract has no header-name or
//! header-value type because neither is representable across a WASM boundary (see [`super`]). Two
//! checks therefore happen on this side of that boundary, and they happen here — once, for both the
//! live request and response [`PluginContext`](super::PluginContext) exposes and the
//! [`ResponseData`](super::ResponseData) a plugin synthesizes:
//!
//! - **A name is an RFC 9110 token.** Pingora's header map stores a name as opaque bytes, so an
//!   unvalidated name would reach the request line or the response head and produce a message no
//!   peer can parse.
//! - **A value is visible ASCII, space or tab.** That is exactly the set a value can be read back
//!   as, so a plugin never writes a value it could not have read, and the writer cannot fail on a
//!   value this check accepted.
//!
//! The hop-by-hop list lives here too. Which headers describe a single connection rather than the
//! message is one rule, and "what a plugin may write" and "what the data plane strips" must not be
//! able to disagree about it.

#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the five-phase contract lands before its caller; the hook rewrite wires this in"
    )
)]

/// Headers that describe one connection and are never forwarded nor synthesized.
///
/// The framework already strips these from an upstream request (see `pingora-guidelines.md`); the
/// list exists here because a *synthesized* response does not pass through that hook, so the data
/// plane has to apply the same rule to bytes it produces itself.
pub(crate) const HOP_BY_HOP: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Reports whether `name` describes the connection rather than the message.
///
/// The comparison is case-insensitive: header names are case-insensitive on the wire, and a plugin
/// writing `Transfer-Encoding` must be treated exactly like one writing `transfer-encoding`.
pub(crate) fn is_hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|hop| name.eq_ignore_ascii_case(hop))
}

/// Reports whether `name` is an RFC 9110 `token`, the grammar of a header field name.
pub(crate) fn is_header_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(is_token_byte)
}

/// Reports whether `value` is a header field value this contract can carry.
///
/// The accepted set is visible ASCII, space and horizontal tab: every byte a value can hold and
/// still be read back as a `String`. Control bytes, CR and LF in particular, are refused — a stored
/// CR LF is a response-splitting bug waiting for the code that serializes it.
pub(crate) fn is_header_value(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
}

/// Splits a `connection` header value into the header names it nominates.
///
/// A `Connection` value is a comma-separated list of tokens naming headers that are specific to this
/// connection; RFC 9110 requires them to be removed before the message is forwarded or stored. Empty
/// entries are skipped, so a trailing comma cannot nominate the empty name.
pub(crate) fn connection_tokens(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(',')
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// Reports whether `byte` may appear in a header field name (`tchar`).
const fn is_token_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'!' | b'#'
            | b'$'
            | b'%'
            | b'&'
            | b'\''
            | b'*'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~'
    ) || byte.is_ascii_digit()
        || byte.is_ascii_alphabetic()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_names_a_header_and_nothing_else_does() {
        for name in ["x-api-key", "Content-Type", "x_custom.1", "a~b"] {
            assert!(is_header_name(name), "`{name}` is a token");
        }
        for name in ["", "bad name", "x:y", "line\r\nbreak", "ünïcode"] {
            assert!(!is_header_name(name), "`{name}` is not a token");
        }
    }

    #[test]
    fn a_value_holds_visible_ascii_space_and_tab() {
        assert!(is_header_value(""));
        assert!(is_header_value("application/json; charset=utf-8"));
        assert!(is_header_value("a\tb"));
        for value in ["a\r\nb", "a\nb", "a\0b", "déjà"] {
            assert!(!is_header_value(value), "`{value}` is not a field value");
        }
    }

    #[test]
    fn the_hop_by_hop_set_is_case_insensitive() {
        assert!(is_hop_by_hop("Connection"));
        assert!(is_hop_by_hop("TRANSFER-ENCODING"));
        assert!(!is_hop_by_hop("content-type"));
    }

    #[test]
    fn a_connection_value_nominates_every_non_empty_token() {
        let nominated: Vec<&str> = connection_tokens("x-keep-alive, Keep-Out,, ").collect();
        assert_eq!(nominated, ["x-keep-alive", "Keep-Out"]);
    }
}
