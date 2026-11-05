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

use pingora::http::HMap;

use super::PluginError;

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

/// Rejects a write to a header the data plane frames a message with.
///
/// A synthetic response has its `content-length` recomputed and a forwarded one has its framing
/// restored, so a plugin write here could only ever be overwritten — or, worse, believed by a reader.
/// Answering with a refusal keeps the rule visible at the call site instead of leaving a silent no-op.
///
/// # Errors
/// Returns [`PluginError::Execute`] when `name` is `content-length` or `transfer-encoding`.
pub(crate) fn reject_framing(name: &str) -> Result<(), PluginError> {
    if is_framing(name) {
        return Err(PluginError::Execute(
            "the data plane owns `content-length` and `transfer-encoding`",
        ));
    }
    Ok(())
}

/// Reports whether `name` is a framing header the data plane owns.
///
/// The head must describe the bytes that follow it, and only the data plane knows those. A plugin
/// writing either of these would desynchronise a message from its own body, which is why every write
/// path refuses them instead of trusting a later correction.
pub(crate) fn is_framing(name: &str) -> bool {
    name.eq_ignore_ascii_case("content-length") || name.eq_ignore_ascii_case("transfer-encoding")
}

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

/// Reads the first value of one header, or `None` when it is absent or not UTF-8.
///
/// A value that is not valid UTF-8 reads as absent on purpose: the contract's value type is a
/// `String`, and handing a plugin a mangled copy of a credential is worse than telling it there is
/// none. An authentication plugin then answers "missing", which is the closed answer.
pub(crate) fn first_value(headers: &HMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Validates one header name and value, and returns them in the owned form a header map stores.
///
/// The write path and the synthesised-response constructor share it, so "a name is a token and a value
/// is visible ASCII" has one spelling and one message.
///
/// # Errors
/// Returns [`PluginError::Execute`] when `name` is not an RFC 9110 token or `value` contains a byte
/// that cannot be read back as a field value.
pub(crate) fn validated(name: &str, value: &str) -> Result<(String, String), PluginError> {
    if !is_header_name(name) {
        return Err(PluginError::Execute(
            "a header name must be an RFC 9110 token",
        ));
    }
    if !is_header_value(value) {
        return Err(PluginError::Execute(
            "a header value must be visible ASCII, space or tab",
        ));
    }
    Ok((name.to_owned(), value.to_owned()))
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
    fn a_validated_pair_is_owned_and_a_broken_one_names_the_rule() {
        let (name, value) = validated("x-api-key", "abc").expect("the pair is valid");
        assert_eq!((name.as_str(), value.as_str()), ("x-api-key", "abc"));

        let error = validated("bad name", "abc").expect_err("a name must be a token");
        assert!(
            error.to_string().contains("RFC 9110 token"),
            "the message must name the rule: {error}"
        );
        let error = validated("x-api-key", "a\nb").expect_err("a value must be visible ASCII");
        assert!(
            error.to_string().contains("visible ASCII"),
            "the message must name the rule: {error}"
        );
    }

    #[test]
    fn a_first_value_reads_utf8_only() {
        let mut headers = HMap::new();
        assert_eq!(first_value(&headers, "x-api-key"), None);
        headers.insert(
            "x-api-key",
            "abc".parse().expect("the fixture value parses"),
        );
        assert_eq!(first_value(&headers, "x-api-key").as_deref(), Some("abc"));
    }

    #[test]
    fn a_connection_value_nominates_every_non_empty_token() {
        let nominated: Vec<&str> = connection_tokens("x-keep-alive, Keep-Out,, ").collect();
        assert_eq!(nominated, ["x-keep-alive", "Keep-Out"]);
    }
}
