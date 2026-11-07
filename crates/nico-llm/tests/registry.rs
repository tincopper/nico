//! The kinds one process serves: the pair of registries the process assembly is handed.
//!
//! Publication pairs a route's `protocol` with a codec and its provider's `kind` with an adapter, so
//! both lookups are the contract this file asserts. It also pins the failure a duplicate registration
//! returns, because the process can only skip what it is told about.

use std::sync::Arc;

use nico_llm::{
    ClientCodec, Provider, RegistryError, builtin_registry,
    protocol::{
        anthropic::{AnthropicMessagesCodec, PROTOCOL as ANTHROPIC_MESSAGES},
        openai_chat::{OpenAiChatCodec, PROTOCOL},
        openai_responses::{OpenAiResponsesCodec, PROTOCOL as RESPONSES_MESSAGES},
    },
    provider::{
        anthropic::{NATIVE_PROTOCOL as ANTHROPIC_PROTOCOL, PROVIDER_KIND as ANTHROPIC_KIND},
        gemini::{NATIVE_PROTOCOL as GEMINI_PROTOCOL, PROVIDER_KIND as GEMINI_KIND},
        openai::PROVIDER_KIND,
        openai_responses::{
            NATIVE_PROTOCOL as RESPONSES_PROTOCOL, PROVIDER_KIND as RESPONSES_KIND,
        },
    },
};

#[test]
fn the_builtin_registry_answers_both_lookups_a_publication_makes() {
    let (protocols, providers) = builtin_registry();

    let codec = protocols
        .codec(PROTOCOL.as_str())
        .expect("the built-in client protocol resolves");
    assert_eq!(codec.protocol(), PROTOCOL);
    let messages = protocols
        .codec(ANTHROPIC_MESSAGES.as_str())
        .expect("the registered Anthropic codec resolves");
    assert_eq!(messages.protocol(), ANTHROPIC_MESSAGES);
    assert_eq!(
        messages.version(),
        Some(nico_llm::protocol::anthropic::VERSION),
        "the assembly log reports the wire revision this build speaks"
    );
    let responses = protocols
        .codec(RESPONSES_MESSAGES.as_str())
        .expect("the registered Responses codec resolves");
    assert_eq!(responses.protocol(), RESPONSES_MESSAGES);
    assert_eq!(
        responses.version(),
        None,
        "this vendor has no dated spelling for the dialect, so the log leaves the field out"
    );
    assert_eq!(
        protocols.len(),
        3,
        "this build serves three client protocols"
    );
    assert!(!protocols.is_empty());

    let adapter = providers
        .provider(PROVIDER_KIND.as_str())
        .expect("the built-in provider kind resolves");
    assert_eq!(adapter.kind(), PROVIDER_KIND);

    let anthropic = providers
        .provider(ANTHROPIC_KIND.as_str())
        .expect("the registered Anthropic adapter resolves");
    assert_eq!(anthropic.kind(), ANTHROPIC_KIND);
    assert_eq!(
        anthropic.native_protocol(),
        ANTHROPIC_PROTOCOL,
        "a passthrough route compares this with the client codec's protocol"
    );

    let responses = providers
        .provider(RESPONSES_KIND.as_str())
        .expect("the registered Responses adapter resolves");
    assert_eq!(responses.kind(), RESPONSES_KIND);
    assert_eq!(
        responses.native_protocol(),
        RESPONSES_PROTOCOL,
        "one kind names one dialect, so the second OpenAI dialect is a second kind"
    );
    let gemini = providers
        .provider(GEMINI_KIND.as_str())
        .expect("the registered Gemini adapter resolves");
    assert_eq!(gemini.kind(), GEMINI_KIND);
    assert_eq!(
        gemini.native_protocol(),
        GEMINI_PROTOCOL,
        "one kind names one dialect, so the equality a passthrough route needs is still equality"
    );
    assert_eq!(providers.len(), 4, "this build serves four provider kinds");
    assert!(!providers.is_empty());
}

#[test]
fn an_unknown_key_is_absent_rather_than_guessed() {
    let (protocols, providers) = builtin_registry();

    assert!(
        protocols.codec("openai.chat.completions").is_none(),
        "a registry answers the exact key and nothing else"
    );
    assert!(providers.provider("OpenAI").is_none());
    assert!(providers.provider("").is_none());
    assert!(
        providers.provider("messages").is_none(),
        "the kind is `anthropic`; a wire dialect name is not a registry key"
    );
    assert!(
        providers.provider("openai.chat").is_none(),
        "a protocol identifier is not a provider kind"
    );
}

#[test]
fn a_duplicate_registration_is_refused_rather_than_overwritten() {
    let (mut protocols, mut providers) = builtin_registry();
    let codec: Arc<dyn ClientCodec> = Arc::new(OpenAiChatCodec::new());
    let messages: Arc<dyn ClientCodec> = Arc::new(AnthropicMessagesCodec::new());
    let responses: Arc<dyn ClientCodec> = Arc::new(OpenAiResponsesCodec::new());
    let adapter: Arc<dyn Provider> = Arc::new(nico_llm::provider::OpenAiProvider::new());

    assert_eq!(
        protocols.register(codec),
        Err(RegistryError::DuplicateKind {
            kind: PROTOCOL.as_str().to_owned(),
        }),
        "which codec a protocol reached would otherwise depend on assembly order"
    );
    assert_eq!(
        protocols.register(messages),
        Err(RegistryError::DuplicateKind {
            kind: ANTHROPIC_MESSAGES.as_str().to_owned(),
        }),
        "a second codec for one protocol is refused for the same reason"
    );
    assert_eq!(
        protocols.register(responses),
        Err(RegistryError::DuplicateKind {
            kind: RESPONSES_MESSAGES.as_str().to_owned(),
        })
    );
    assert_eq!(
        providers.register(adapter),
        Err(RegistryError::DuplicateKind {
            kind: PROVIDER_KIND.as_str().to_owned(),
        })
    );
    assert_eq!(
        protocols.len(),
        3,
        "the refused registration changed nothing"
    );
    assert_eq!(providers.len(), 4);
}
