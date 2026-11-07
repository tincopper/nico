//! The error envelope a Responses caller parses, and the one it shares with the Chat dialect.

use nico_core::ErrorCategory;
use nico_llm::{protocol::ClientCodec, schema::CanonicalError};

/// Encodes one failure with `codec` and returns the parsed document.
fn envelope(codec: &dyn ClientCodec, category: ErrorCategory) -> serde_json::Value {
    let error = CanonicalError::try_new(category, "some_stable_code", "a safe message", 400)
        .expect("a valid error");
    let body = codec
        .encode_error(&error)
        .expect("the envelope is encodable");
    serde_json::from_slice(&body).expect("the envelope is a JSON document")
}

#[test]
fn the_envelope_is_the_one_both_openai_protocols_answer_with() {
    let responses = envelope(&super::support::codec(), ErrorCategory::Client);
    let chat: Box<dyn ClientCodec> =
        Box::new(nico_llm::protocol::openai_chat::OpenAiChatCodec::new());
    let chat = envelope(chat.as_ref(), ErrorCategory::Client);

    assert_eq!(responses["error"]["type"], "invalid_request_error");
    assert_eq!(responses["error"]["code"], "some_stable_code");
    assert_eq!(responses, chat, "one vendor, one envelope");
}

#[test]
fn every_category_has_a_word() {
    for (category, expected) in [
        (ErrorCategory::Client, "invalid_request_error"),
        (ErrorCategory::Limit, "rate_limit_error"),
        (ErrorCategory::Upstream, "server_error"),
        (ErrorCategory::Internal, "server_error"),
        (ErrorCategory::Cancelled, "server_error"),
    ] {
        let document = envelope(&super::support::codec(), category);
        assert_eq!(document["error"]["type"], expected, "{category:?}");
    }
}
