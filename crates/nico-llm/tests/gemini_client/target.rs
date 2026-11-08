//! The request target: the model and the operation are facts of the path, not of the body.

use nico_llm::{
    CodecError, Provider, ProviderRoute,
    provider::gemini::{GeminiProvider, NATIVE_PROTOCOL, PROVIDER_KIND},
};

use crate::support::{self, BUFFERED, MODEL, STREAMING, facts_at, target};

/// One turn of text, the smallest document that decodes.
const BODY: &str = r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#;

/// The operation decides the canonical stream flag, and the path names the canonical model.
#[test]
fn the_operation_sets_the_stream_flag_and_the_path_names_the_model() {
    let buffered = support::decode(BODY).expect("a buffered target decodes");
    assert_eq!(buffered.model().as_str(), MODEL);
    assert!(
        !buffered.stream(),
        "generateContent is not the streaming operation"
    );

    let streamed =
        support::decode_at(&facts_at(&target(STREAMING)), BODY).expect("a streamed target decodes");
    assert!(
        streamed.stream(),
        "streamGenerateContent is the streaming operation"
    );
    assert_eq!(streamed.model().as_str(), MODEL);
}

/// `alt=sse` is the one answer shape this gateway streams; other spellings are refused by name.
#[test]
fn alt_is_absent_or_sse_and_nothing_else() {
    support::decode_at(&facts_at(&target("streamGenerateContent")), BODY)
        .expect("a stream request without alt is the default spelling");

    let error = support::decode_at(&facts_at(&target("streamGenerateContent?alt=json")), BODY)
        .expect_err("a non-sse answer shape is refused");
    assert!(
        matches!(error, CodecError::UnsupportedField { ref field, .. } if field == "alt"),
        "got {error:?}"
    );
}

/// A query parameter the canonical request cannot carry is refused by name, value unnamed.
#[test]
fn an_unknown_query_parameter_is_refused_and_its_value_never_quoted() {
    let error = support::decode_at(
        &facts_at("/v1beta/models/gemini-2.0-flash:generateContent?key=S3CR3T&alt=sse"),
        BODY,
    )
    .expect_err("a query credential must not travel into a preserved bucket");
    let CodecError::UnsupportedField { field, .. } = &error else {
        panic!("expected UnsupportedField, got {error:?}");
    };
    assert_eq!(field.as_ref(), "query.key");
    assert!(
        !error.to_string().contains("S3CR3T"),
        "a refusal names the field, never its value: {error}"
    );
}

/// The dialect's other operations answer different questions, so they are bad documents here.
#[test]
fn another_operation_is_refused() {
    for operation in ["countTokens", "embedContent", "predict", ""] {
        let error = support::decode_at(&facts_at(&target(operation)), BODY)
            .expect_err("a non-generation operation is refused");
        assert!(
            matches!(error, CodecError::InvalidRequest { .. }),
            "`{operation}` is a bad document, got {error:?}"
        );
    }
}

/// Targets that do not separate a model from an operation are bad documents.
#[test]
fn a_target_without_both_facts_is_refused() {
    for bad in [
        "/v1beta/models",
        "/v1beta/models/gemini-2.0-flash",
        "/v1beta/models/:generateContent",
        "/v1/chat/completions",
    ] {
        let error = support::decode_at(&facts_at(bad), BODY)
            .expect_err("a target without both facts is refused");
        assert!(
            matches!(error, CodecError::InvalidRequest { .. }),
            "`{bad}` is a bad document, got {error:?}"
        );
    }
}

/// Paths with escapes or multibyte prefixes parse or refuse by type; they never panic.
#[test]
fn an_unusual_path_never_panics() {
    let escaped = support::decode_at(
        &facts_at("/v1beta/models/gemini%2Fpro:generateContent"),
        BODY,
    )
    .expect("an escaped name is one literal model name");
    assert_eq!(escaped.model().as_str(), "gemini%2Fpro");

    let multibyte = support::decode_at(
        &facts_at("/v1β/models/gemini-2.0-flash:generateContent"),
        BODY,
    )
    .expect("a prefix the deployment owns is not this codec's concern");
    assert_eq!(multibyte.model().as_str(), MODEL);
}

/// The convert round trip: what the client sent is what the provider encodes, one document.
///
/// The body below uses only shapes both sides of the dialect write, so the codec's decode followed
/// by the adapter's encode reproduces the request byte for byte in value.
#[test]
fn the_client_document_is_what_the_provider_encodes() {
    let wire = serde_json::json!({
        "systemInstruction": {"parts": [{"text": "be brief"}]},
        "contents": [
            {"role": "user", "parts": [
                {"text": "look"},
                {"inlineData": {"mimeType": "image/png", "data": "aGVsbG8="}}
            ]},
            {"role": "model", "parts": [
                {"text": "calling"},
                {"functionCall": {"id": "call_0", "name": "lookup", "args": {"q": "gemini"}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"id": "call_0", "name": "lookup", "response": {"content": "found"}}}
            ]}
        ],
        "generationConfig": {"temperature": 0.5, "maxOutputTokens": 64, "stopSequences": ["END"]},
        "labels": {"team": "gateway"}
    });
    let request = support::decode_at(&facts_at(&target(BUFFERED)), &wire.to_string())
        .expect("the round-trip document decodes");

    let route = ProviderRoute::try_new(
        "gemini",
        NATIVE_PROTOCOL,
        PROVIDER_KIND,
        "http://127.0.0.1:8080",
    )
    .expect("a valid route")
    .with_model_map([(MODEL.to_owned(), MODEL.to_owned())].into_iter().collect())
    .expect("a valid model map");
    let encoded = GeminiProvider::new()
        .encode(&route, &request)
        .expect("the adapter encodes what the codec decoded");

    let encoded: serde_json::Value = serde_json::from_slice(&encoded).expect("a JSON document");
    assert_eq!(encoded, wire, "one conversation, one spelling per dialect");
}
