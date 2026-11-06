//! The Chat Completions codec as a whole: what it declares, and how it renders a failure.

#![expect(
    clippy::expect_used,
    reason = "a test that cannot build its fixture has nothing useful to return"
)]

mod message;
mod request;
mod response;
mod stream;
mod support;

use nico_core::ErrorCategory;
use nico_llm::{CanonicalError, Capability, ClientCodec};
use support::{codec, json};

#[test]
fn the_codec_declares_the_capabilities_it_actually_maps() {
    let codec = codec();
    assert_eq!(codec.protocol().as_str(), "openai.chat");
    let capabilities = codec.capabilities();
    for capability in [
        Capability::Streaming,
        Capability::Tools,
        Capability::Images,
        Capability::Reasoning,
        Capability::ResponseFormat,
    ] {
        assert!(
            capabilities.supports(capability),
            "`{capability}` is mapped and must be declared"
        );
    }
    assert!(
        !capabilities.supports(Capability::Files),
        "a file reference has no canonical form yet, so the capability must not be declared"
    );
}

#[test]
fn an_error_is_rendered_in_the_protocols_own_envelope() {
    let error = CanonicalError::try_new(
        ErrorCategory::Client,
        "unsupported_capability",
        "the route does not support a capability this request needs",
        422,
    )
    .expect("a valid canonical error");
    let body = codec().encode_error(&error).expect("the error encodes");
    let document = json(std::str::from_utf8(&body).expect("the body is UTF-8"));
    assert_eq!(document["error"]["code"], "unsupported_capability");
    assert_eq!(
        document["error"]["message"],
        "the route does not support a capability this request needs"
    );
    assert_eq!(document["error"]["type"], "invalid_request_error");
    assert!(
        document["error"]["param"].is_null(),
        "the gateway does not guess which parameter failed"
    );
    assert_eq!(
        body,
        codec()
            .encode_error(&error)
            .expect("encoding is deterministic"),
        "the same failure must render the same bytes"
    );
}

#[test]
fn an_upstream_failure_is_rendered_as_a_server_error() {
    let error = CanonicalError::try_new(
        ErrorCategory::Upstream,
        "provider_overloaded",
        "the provider is temporarily overloaded",
        503,
    )
    .expect("a valid canonical error");
    let body = codec().encode_error(&error).expect("the error encodes");
    let document = json(std::str::from_utf8(&body).expect("the body is UTF-8"));
    assert_eq!(document["error"]["type"], "server_error");
    assert_eq!(document["error"]["code"], "provider_overloaded");
    assert!(
        !document.to_string().contains("Authorization"),
        "an error envelope never carries a credential"
    );
}
