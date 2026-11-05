//! The refusal one plugin instance presents, compiled once from its document entry.
//!
//! A rejection is **data**: a status, a fixed stable code, a content type and a body template. It is
//! built while the document is published and rendered on the request path without deciding anything,
//! so [`HttpError::respond`] stays the only place that maps a failure to a wire response and
//! `proxy::failure::write_respond` stays the only writer.
//!
//! Three rules are enforced here, and all three are compile-time:
//!
//! - **The stable code is not configurable.** `auth_missing`, `auth_invalid` and `rate_limited` are
//!   what a caller matches on, so `error_response` overrides the status, the content type and the
//!   body — never the code `x-nico-error-code` carries.
//! - **A template names only whitelisted variables.** `${code}`, `${message}` and `${trace_id}` are
//!   the whole set: upstream names, configuration paths and credentials have no spelling, so "a
//!   rejection never echoes internal detail" is a property of the compiler rather than a rule a
//!   reviewer has to check.
//! - **A body's shape follows its content type.** A JSON content type carries a structured value and
//!   every substituted variable is escaped; any other content type carries a literal template. The
//!   built-in envelope is JSON, so a non-JSON content type with no body is refused instead of
//!   labelling one shape as the other.
//!
//! [`HttpError::respond`]: nico_http::HttpError::respond

use std::sync::Arc;

use nico_core::{
    BodyValue, DEFAULT_CONTENT_TYPE, ErrorBody, ErrorCategory, ErrorResponse, is_json_content_type,
};
use nico_http::{BodyFormat, BodyTemplate, Part, Rejection, RejectionError, escape_json_string};

use crate::PluginError;

/// Stable code of a request that presented no credential.
const AUTH_MISSING_CODE: &str = "auth_missing";
/// Caller-safe message of a request that presented no credential.
const AUTH_MISSING_MESSAGE: &str = "no credential was presented";
/// Stable code of a credential that was presented and refused.
///
/// It is deliberately the *only* authentication failure code: distinguishing "expired" from "wrong
/// signature" from "unknown key" would let a caller enumerate which keys exist.
const AUTH_INVALID_CODE: &str = "auth_invalid";
/// Caller-safe message of a credential that was presented and refused.
const AUTH_INVALID_MESSAGE: &str = "the credential is not accepted";
/// Stable code of a request over its rate limit.
const RATE_LIMITED_CODE: &str = "rate_limited";
/// Caller-safe message of a request over its rate limit.
const RATE_LIMITED_MESSAGE: &str = "the request rate exceeds the limit for this route";

/// Status a rejection defaults to when the entry configures none.
const UNAUTHORIZED: u16 = 401;
/// Status a rate limit defaults to when the entry configures none.
const TOO_MANY_REQUESTS: u16 = 429;
/// The challenge a 401 answers with: how to authenticate, per RFC 6750.
const BEARER: &str = "Bearer";

/// Longest variable name this compiler will quote back in an error message.
///
/// The name comes from a document, so reporting it verbatim would let one configuration put a control
/// character into a compile log. A name longer than any accepted variable is truncated rather than
/// repeated.
const MAX_REPORTED_VARIABLE: usize = 32;

/// The two refusals one authentication plugin presents.
///
/// They are compiled together because they share a status, a content type and a body shape, and
/// because the difference between them is the one decision the filter makes: was a credential
/// presented at all. Both are `Arc`-shared by every request the plugin refuses.
#[derive(Clone)]
pub(crate) struct AuthRejections {
    missing: Arc<Rejection>,
    invalid: Arc<Rejection>,
}

impl AuthRejections {
    /// Returns the refusal for a request that presented no credential.
    pub(crate) fn missing(&self) -> Arc<Rejection> {
        Arc::clone(&self.missing)
    }

    /// Returns the refusal for a credential that was presented and refused.
    pub(crate) fn invalid(&self) -> Arc<Rejection> {
        Arc::clone(&self.invalid)
    }
}

/// Compiles both refusals of an authentication plugin: missing and invalid credential.
///
/// # Errors
/// - [`PluginError::Rejection`] when a presentation cannot be written (status, header value, template
///   size).
/// - [`PluginError::UnknownVariable`] / [`PluginError::UnterminatedTemplate`] when the configured body
///   names something outside the whitelist.
/// - [`PluginError::ContentTypeWithoutBody`] when a non-JSON content type has no body.
pub(crate) fn auth(
    plugin: &str,
    response: Option<&ErrorResponse>,
) -> Result<AuthRejections, PluginError> {
    let missing = compile(
        plugin,
        AUTH_MISSING_CODE,
        AUTH_MISSING_MESSAGE,
        UNAUTHORIZED,
        ErrorCategory::Client,
        Some(BEARER),
        response,
    )?;
    let invalid = compile(
        plugin,
        AUTH_INVALID_CODE,
        AUTH_INVALID_MESSAGE,
        UNAUTHORIZED,
        ErrorCategory::Client,
        Some(BEARER),
        response,
    )?;
    Ok(AuthRejections {
        missing: Arc::new(missing),
        invalid: Arc::new(invalid),
    })
}

/// Compiles the refusal of a rate-limit plugin.
///
/// # Errors
/// The same failures as [`auth`], minus the challenge: a rate limit is not an authentication failure,
/// so it must not tell the caller how to authenticate.
pub(crate) fn rate_limit(
    plugin: &str,
    response: Option<&ErrorResponse>,
) -> Result<Arc<Rejection>, PluginError> {
    compile(
        plugin,
        RATE_LIMITED_CODE,
        RATE_LIMITED_MESSAGE,
        TOO_MANY_REQUESTS,
        ErrorCategory::Limit,
        None,
        response,
    )
    .map(Arc::new)
}

/// Compiles one presentation: a document override layered over the kind's built-in defaults.
fn compile(
    plugin: &str,
    code: &'static str,
    message: &'static str,
    default_status: u16,
    category: ErrorCategory,
    challenge: Option<&'static str>,
    response: Option<&ErrorResponse>,
) -> Result<Rejection, PluginError> {
    let status = response
        .and_then(ErrorResponse::status)
        .unwrap_or(default_status);
    let content_type = response.map_or(DEFAULT_CONTENT_TYPE, ErrorResponse::content_type);
    let body = body(plugin, response)?;
    let rejection = Rejection::from_u16(status, code, message, content_type, category, body)
        .map_err(|source| PluginError::Rejection {
            plugin: plugin.to_owned(),
            source,
        })?;
    match challenge {
        Some(challenge) => {
            rejection
                .with_challenge(challenge)
                .map_err(|source| PluginError::Rejection {
                    plugin: plugin.to_owned(),
                    source,
                })
        }
        None => Ok(rejection),
    }
}

/// Compiles the body template an entry describes, or the built-in envelope when it describes none.
fn body(plugin: &str, response: Option<&ErrorResponse>) -> Result<BodyTemplate, PluginError> {
    let Some(response) = response else {
        return Ok(BodyTemplate::error_envelope());
    };
    let json = is_json_content_type(response.content_type());
    match response.body() {
        Some(ErrorBody::Json(value)) => json_template(plugin, value),
        Some(ErrorBody::Text(_)) if json => {
            // Unreachable for a document this build decoded (the shape follows the content type), so
            // reaching it means the model changed under this crate: refuse rather than label a text
            // body as JSON.
            Err(mismatched_shape(plugin))
        }
        Some(ErrorBody::Text(text)) => text_template(plugin, text),
        // The built-in envelope is JSON, so another content type has nothing to describe it.
        None if !json => Err(PluginError::ContentTypeWithoutBody {
            plugin: plugin.to_owned(),
        }),
        None => Ok(BodyTemplate::error_envelope()),
        // A body kind this build does not know has no interpretation, and guessing one would write
        // a body the operator did not ask for.
        Some(_) => Err(mismatched_shape(plugin)),
    }
}

/// Builds the failure for a body whose shape its content type does not allow.
fn mismatched_shape(plugin: &str) -> PluginError {
    PluginError::InvalidParameter {
        plugin: plugin.to_owned(),
        field: "error_response.body",
        reason: "must be a structured value for a JSON content type and a string otherwise".into(),
    }
}

/// Compiles a literal body template: the whole string is text with variables between it.
fn text_template(plugin: &str, text: &str) -> Result<BodyTemplate, PluginError> {
    let mut parts = Vec::new();
    for piece in parse(plugin, text)? {
        parts.push(match piece {
            Piece::Text(literal) => Part::Literal(literal.into()),
            Piece::Code => Part::Code,
            Piece::Message => Part::Message,
            Piece::TraceId => Part::TraceId,
        });
    }
    BodyTemplate::new(BodyFormat::Text, parts).map_err(|source| PluginError::Rejection {
        plugin: plugin.to_owned(),
        source,
    })
}

/// Compiles a structured body into JSON syntax plus the variables its strings contain.
fn json_template(plugin: &str, value: &BodyValue) -> Result<BodyTemplate, PluginError> {
    let mut parts = Vec::new();
    json_parts(plugin, value, &mut parts)?;
    BodyTemplate::new(BodyFormat::Json, parts).map_err(|source| PluginError::Rejection {
        plugin: plugin.to_owned(),
        source,
    })
}

/// Renders one structured value into JSON syntax, keeping its strings as templates.
fn json_parts(plugin: &str, value: &BodyValue, parts: &mut Vec<Part>) -> Result<(), PluginError> {
    match value {
        BodyValue::Null => parts.push(Part::Literal("null".into())),
        BodyValue::Bool(true) => parts.push(Part::Literal("true".into())),
        BodyValue::Bool(false) => parts.push(Part::Literal("false".into())),
        // The document model converts a number into the spelling it is rendered with, so it is already
        // JSON syntax; re-rendering it would need a number formatter this crate does not have.
        BodyValue::Number(number) => parts.push(Part::Literal(number.clone().into_boxed_str())),
        BodyValue::String(text) => json_string(plugin, text, parts)?,
        BodyValue::Array(items) => {
            parts.push(Part::Literal("[".into()));
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    parts.push(Part::Literal(",".into()));
                }
                json_parts(plugin, item, parts)?;
            }
            parts.push(Part::Literal("]".into()));
        }
        BodyValue::Object(entries) => {
            parts.push(Part::Literal("{".into()));
            for (index, (key, item)) in entries.iter().enumerate() {
                if index > 0 {
                    parts.push(Part::Literal(",".into()));
                }
                json_string(plugin, key, parts)?;
                parts.push(Part::Literal(":".into()));
                json_parts(plugin, item, parts)?;
            }
            parts.push(Part::Literal("}".into()));
        }
        // A value kind this build does not know has no JSON spelling here.
        _ => return Err(mismatched_shape(plugin)),
    }
    Ok(())
}

/// Renders one JSON string: quoted syntax around escaped literals and unescaped variables.
///
/// The syntax this function writes is not escaped — it is the document's own structure. Only the
/// text the document supplied inside the string is, because that text may contain a quote or a
/// control character. Variables are left to the renderer, which escapes every value it substitutes.
fn json_string(plugin: &str, text: &str, parts: &mut Vec<Part>) -> Result<(), PluginError> {
    parts.push(Part::Literal("\"".into()));
    for piece in parse(plugin, text)? {
        match piece {
            Piece::Text(literal) => {
                parts.push(Part::Literal(escape_json_string(literal).into()));
            }
            Piece::Code => parts.push(Part::Code),
            Piece::Message => parts.push(Part::Message),
            Piece::TraceId => parts.push(Part::TraceId),
        }
    }
    parts.push(Part::Literal("\"".into()));
    Ok(())
}

/// One piece of a parsed template.
enum Piece<'a> {
    /// Text between two variables, or the whole string when it has none.
    Text(&'a str),
    /// `${code}`.
    Code,
    /// `${message}`.
    Message,
    /// `${trace_id}`.
    TraceId,
}

/// Splits a template into literal text and whitelisted variables.
///
/// There is no escape for a literal `${`: every occurrence opens a placeholder, because a template
/// language with two meanings for one sequence is one an operator gets wrong silently. Text that has
/// to contain it is not expressible in a template, and a name outside the whitelist is refused rather
/// than written through.
///
/// # Errors
/// - [`PluginError::UnknownVariable`] when `${...}` names anything outside the whitelist.
/// - [`PluginError::UnterminatedTemplate`] when a `${` is never closed.
fn parse<'a>(plugin: &str, text: &'a str) -> Result<Vec<Piece<'a>>, PluginError> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let body = &rest[start + 2..];
        let Some(end) = body.find('}') else {
            return Err(PluginError::UnterminatedTemplate {
                plugin: plugin.to_owned(),
            });
        };
        if start > 0 {
            pieces.push(Piece::Text(&rest[..start]));
        }
        pieces.push(variable(plugin, &body[..end])?);
        rest = &body[end + 1..];
    }
    if !rest.is_empty() {
        pieces.push(Piece::Text(rest));
    }
    Ok(pieces)
}

/// Maps one variable name onto its [`Piece`], refusing everything outside the whitelist.
fn variable(plugin: &str, name: &str) -> Result<Piece<'static>, PluginError> {
    match name {
        "code" => Ok(Piece::Code),
        "message" => Ok(Piece::Message),
        "trace_id" => Ok(Piece::TraceId),
        other => Err(PluginError::UnknownVariable {
            plugin: plugin.to_owned(),
            variable: reportable(other),
        }),
    }
}

/// Returns a spelling of a document-supplied name that is safe to put in a log line.
///
/// The name is only ever reported to explain a configuration mistake, so a control character or a
/// non-ASCII lookalike is replaced instead of repeated, and the result is bounded.
fn reportable(raw: &str) -> String {
    raw.chars()
        .take(MAX_REPORTED_VARIABLE)
        .map(|character| {
            if character.is_ascii_graphic() || character == ' ' {
                character
            } else {
                '?'
            }
        })
        .collect()
}

// Compiled-in proof that the compiled refusal a filter shares can cross `.await` and threads.
const _: fn() = || {
    const fn assert_send_sync_static<T: Send + Sync + 'static>() {}
    assert_send_sync_static::<AuthRejections>();
    assert_send_sync_static::<RejectionError>();
};

#[cfg(test)]
mod tests;
