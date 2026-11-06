//! The kinds one process serves: the pair of registries the process assembly is handed.
//!
//! Publication pairs a route's `protocol` with a codec and its provider's `kind` with an adapter, so
//! both lookups are the contract this file asserts. It also pins the failure a duplicate registration
//! returns, because the process can only skip what it is told about.

use std::sync::Arc;

use nico_llm::{
    ClientCodec, Provider, RegistryError, builtin_registry,
    protocol::openai_chat::{OpenAiChatCodec, PROTOCOL},
    provider::openai::PROVIDER_KIND,
};

#[test]
fn the_builtin_registry_answers_both_lookups_a_publication_makes() {
    let (protocols, providers) = builtin_registry();

    let codec = protocols
        .codec(PROTOCOL.as_str())
        .expect("the built-in client protocol resolves");
    assert_eq!(codec.protocol(), PROTOCOL);
    assert_eq!(protocols.len(), 1, "this build serves one client protocol");
    assert!(!protocols.is_empty());

    let adapter = providers
        .provider(PROVIDER_KIND.as_str())
        .expect("the built-in provider kind resolves");
    assert_eq!(adapter.kind(), PROVIDER_KIND);
    assert_eq!(providers.len(), 1, "this build serves one provider kind");
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
}

#[test]
fn a_duplicate_registration_is_refused_rather_than_overwritten() {
    let (mut protocols, mut providers) = builtin_registry();
    let codec: Arc<dyn ClientCodec> = Arc::new(OpenAiChatCodec::new());
    let adapter: Arc<dyn Provider> = Arc::new(nico_llm::provider::OpenAiProvider::new());

    assert_eq!(
        protocols.register(codec),
        Err(RegistryError::DuplicateKind {
            kind: PROTOCOL.as_str().to_owned(),
        }),
        "which codec a protocol reached would otherwise depend on assembly order"
    );
    assert_eq!(
        providers.register(adapter),
        Err(RegistryError::DuplicateKind {
            kind: PROVIDER_KIND.as_str().to_owned(),
        })
    );
    assert_eq!(
        protocols.len(),
        1,
        "the refused registration changed nothing"
    );
    assert_eq!(providers.len(), 1);
}
