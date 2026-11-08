//! The wire fixtures: one conversation and one answer, translated by every half of the matrix.
//!
//! `tests/fixtures/matrix/` holds, for each of the four dialects, the client's request document, the
//! client's answer document, the provider's request document and the provider's answer document —
//! all of them translations of the one canonical conversation and answer in [`support`]. Each test
//! here walks one dialect around the whole exchange:
//!
//! 1. the client codec decodes its request document into the canonical request;
//! 2. the client codec encodes the canonical answer into its answer document;
//! 3. the provider adapter encodes the canonical request into its request document;
//! 4. the provider adapter decodes its answer document into the canonical answer.
//!
//! Comparisons are semantic (parsed JSON against parsed JSON, canonical against canonical), so a
//! member's position or a number's spelling cannot make two equal documents unequal. The `ignored`
//! generator test reprints every document, so a deliberate canonical change regenerates the
//! fixtures instead of drifting them.

use std::{fs, path::Path};

use nico_llm::{
    ClientCodec, Provider, ProviderRoute, RequestFacts, ResponseFacts,
    protocol::{
        anthropic::AnthropicMessagesCodec, gemini::GeminiGenerateContentCodec,
        openai_chat::OpenAiChatCodec, openai_responses::OpenAiResponsesCodec,
    },
    provider::{
        anthropic::AnthropicProvider, gemini::GeminiProvider, openai::OpenAiProvider,
        openai_responses::OpenAiResponsesProvider,
    },
};
use serde_json::Value;

use crate::support::{self, assert_request_translates, assert_response_translates};

/// The base URL every fixture route names; no fixture contacts a peer.
const BASE_URL: &str = "http://127.0.0.1:8080";

/// Reads one fixture document.
fn document(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/matrix")
            .join(name),
    )
    .unwrap_or_else(|error| panic!("fixture `{name}` is readable: {error}"))
}

/// Parses one JSON fixture document.
fn json(name: &str) -> Value {
    serde_json::from_str(&document(name))
        .unwrap_or_else(|error| panic!("fixture `{name}` is a JSON document: {error}"))
}

/// Builds the facts of a request whose target carries nothing the codec reads.
fn facts(path: &str) -> RequestFacts {
    RequestFacts::new("POST", path, Vec::new())
}

/// Builds the facts of a provider answer the named media type frames.
fn answer(media_type: &str) -> ResponseFacts {
    ResponseFacts::new(
        200,
        vec![("content-type".to_owned(), media_type.to_owned())],
    )
}

/// Builds the route one fixture adapter compiles: the matrix model map, one output ceiling.
fn route(protocol: nico_llm::ProtocolId, kind: nico_llm::ProviderKind) -> ProviderRoute {
    ProviderRoute::try_new("matrix", protocol, kind, BASE_URL)
        .expect("the fixture route facts are valid")
        .with_model_map(
            [(support::LOGICAL.to_owned(), support::PHYSICAL.to_owned())]
                .into_iter()
                .collect(),
        )
        .expect("a valid model map")
        .with_max_output_tokens(std::num::NonZeroU32::new(64).expect("a positive ceiling"))
}

/// Encodes the canonical request with one adapter and compares it with its fixture.
fn assert_adapter_encodes(provider: &dyn Provider, route: &ProviderRoute, fixture: &str) {
    let wire = provider
        .encode(route, &support::canonical_request())
        .expect("the adapter encodes the canonical request");
    let encoded: Value = serde_json::from_slice(&wire).expect("a JSON document");
    assert_eq!(
        encoded,
        json(fixture),
        "the adapter's request document is the fixture, member for member"
    );
}

/// Decodes one provider answer with one adapter and compares it with the canonical answer.
fn assert_adapter_decodes(provider: &dyn Provider, fixture: &str, media_type: &str) {
    let decoded = provider
        .decode_response(&answer(media_type), document(fixture).as_bytes())
        .expect("the adapter reads its own answer document");
    assert_response_translates(&decoded);
}

/// Encodes the canonical answer with one codec and compares it with its fixture.
fn assert_codec_encodes(codec: &dyn ClientCodec, fixture: &str) {
    let reply = codec
        .encode_response(&support::canonical_response())
        .expect("the codec encodes the canonical answer");
    let encoded: Value = serde_json::from_slice(reply.body()).expect("a JSON document");
    assert_eq!(
        encoded,
        json(fixture),
        "the codec's answer document is the fixture, member for member"
    );
}

/// The OpenAI Chat dialect: the conversation in `messages`, the answer in one `chat.completion`.
#[test]
fn the_openai_chat_dialect_translates_the_conversation() {
    let codec = OpenAiChatCodec::new();
    let decoded = codec
        .decode_request(
            &facts("/v1/chat/completions"),
            document("openai_chat_request.json").as_bytes(),
        )
        .expect("the request document decodes");
    assert_request_translates(&decoded);
    assert_codec_encodes(&codec, "openai_chat_response.json");

    let provider = OpenAiProvider::new();
    let route = route(
        nico_llm::protocol::openai_chat::PROTOCOL,
        nico_llm::provider::openai::PROVIDER_KIND,
    );
    assert_adapter_encodes(&provider, &route, "openai_provider_request.json");
    assert_adapter_decodes(&provider, "openai_provider_answer.json", "application/json");
}

/// The OpenAI Responses dialect: the conversation in `input` items, the answer in `output`.
#[test]
fn the_openai_responses_dialect_translates_the_conversation() {
    let codec = OpenAiResponsesCodec::new();
    let decoded = codec
        .decode_request(
            &facts("/v1/responses"),
            document("openai_responses_request.json").as_bytes(),
        )
        .expect("the request document decodes");
    assert_request_translates(&decoded);
    assert_codec_encodes(&codec, "openai_responses_response.json");

    let provider = OpenAiResponsesProvider::new();
    let route = route(
        nico_llm::protocol::openai_responses::PROTOCOL,
        nico_llm::provider::openai_responses::PROVIDER_KIND,
    );
    assert_adapter_encodes(&provider, &route, "responses_provider_request.json");
    assert_adapter_decodes(
        &provider,
        "responses_provider_answer.json",
        "application/json",
    );
}

/// The Anthropic dialect: the conversation in content blocks, the answer in one `message`.
#[test]
fn the_anthropic_dialect_translates_the_conversation() {
    let codec = AnthropicMessagesCodec::new();
    let decoded = codec
        .decode_request(
            &facts("/v1/messages"),
            document("anthropic_request.json").as_bytes(),
        )
        .expect("the request document decodes");
    assert_request_translates(&decoded);
    assert_codec_encodes(&codec, "anthropic_response.json");

    let provider = AnthropicProvider::new();
    let route = route(
        nico_llm::protocol::anthropic::PROTOCOL,
        nico_llm::provider::anthropic::PROVIDER_KIND,
    );
    assert_adapter_encodes(&provider, &route, "anthropic_provider_request.json");
    assert_adapter_decodes(
        &provider,
        "anthropic_provider_answer.json",
        "application/json",
    );
}

/// The Gemini dialect: the model in the request target, the conversation in `parts`.
#[test]
fn the_gemini_dialect_translates_the_conversation() {
    let codec = GeminiGenerateContentCodec::new();
    let decoded = codec
        .decode_request(
            &facts(&format!(
                "/v1beta/models/{}:generateContent",
                support::LOGICAL
            )),
            document("gemini_request.json").as_bytes(),
        )
        .expect("the request document decodes");
    assert_request_translates(&decoded);
    assert_codec_encodes(&codec, "gemini_response.json");

    let provider = GeminiProvider::new();
    let route = route(
        nico_llm::protocol::gemini::PROTOCOL,
        nico_llm::provider::gemini::PROVIDER_KIND,
    );
    assert_adapter_encodes(&provider, &route, "gemini_provider_request.json");
    assert_adapter_decodes(&provider, "gemini_provider_answer.sse", "text/event-stream");
}

/// Reprints every fixture document, so a deliberate canonical change regenerates the fixtures.
///
/// The output is one labelled JSON document per line group; copy each into
/// `tests/fixtures/matrix/`, read it, and only then commit it — a fixture is an expectation, not
/// whatever the code last printed.
#[test]
#[ignore = "fixture generator: cargo test --test matrix -- --ignored --nocapture"]
fn dump_the_wire_fixtures() {
    let codecs: [(&str, &dyn ClientCodec); 4] = [
        ("openai_chat", &OpenAiChatCodec::new()),
        ("openai_responses", &OpenAiResponsesCodec::new()),
        ("anthropic", &AnthropicMessagesCodec::new()),
        ("gemini", &GeminiGenerateContentCodec::new()),
    ];
    for (name, codec) in codecs {
        println!("=== {name}_response.json");
        let reply = codec
            .encode_response(&support::canonical_response())
            .expect("the canonical answer encodes");
        println!("{}", String::from_utf8(reply.into_body()).expect("UTF-8"));
    }

    let providers: [(&str, &dyn Provider); 4] = [
        ("openai", &OpenAiProvider::new()),
        ("responses", &OpenAiResponsesProvider::new()),
        ("anthropic", &AnthropicProvider::new()),
        ("gemini", &GeminiProvider::new()),
    ];
    for (name, provider) in providers {
        println!("=== {name}_provider_request.json");
        let route = route(provider.native_protocol(), provider.kind());
        let wire = provider
            .encode(&route, &support::canonical_request())
            .expect("the canonical request encodes");
        println!("{}", String::from_utf8(wire).expect("UTF-8"));
    }
}
