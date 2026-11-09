//! Compiling one converting route's attempt chain: its primary target plus every fallback member.
//!
//! The chain is compiled member by member, and **any** member that cannot be served fails the whole
//! route's publication: an operator who declares a fallback has said "if the first target fails, the
//! second must answer", so a second that cannot answer is a route that lies about its own chain. The
//! refusal names the route and the member, and the last known good generation keeps serving.
//!
//! Every member is a full exchange: the route's client codec paired with that member's own adapter,
//! endpoint, model map and credential. The codec is shared — the client protocol is the route's, not
//! a member's — while everything else is per-target endpoint configuration. A member's credential is
//! materialized through the same one-read-per-provider map the primary uses, so a provider named by
//! two routes (or twice by one route) is read once.
//!
//! Each member also records its [`TargetIdentity`]: the facts its breaker state is keyed by across
//! generations. The identity is compiled here because this is the one place that holds both the
//! document's provider entry and the adapter that will actually serve it.

use std::{collections::BTreeMap, sync::Arc};

use nico_core::{Document, Provider, Route};
use nico_llm::{
    AttemptChain, BreakerParams, ChainBuildError, ChainMember, ClientCodec, Credential, Exchange,
    ExchangeMode, ProviderKind, ProviderRegistry, TargetIdentity,
};

use super::{provider_entry, route_facts};
use crate::llm::{CredentialResolver, LlmBuildError};

/// Compiles the route's primary target and its fallback members into one attempt chain.
///
/// Members are compiled in attempt order, so a failure names the first member that cannot be served
/// and stops there — the same first-violation order the document's own validation uses.
///
/// # Errors
/// Returns [`LlmBuildError`] for the first member that cannot be served: a provider the document
/// does not declare, a kind no adapter serves, an unreadable credential, unusable route facts, or an
/// exchange the pair cannot assemble.
pub(super) fn compile_chain(
    document: &Document,
    providers: &ProviderRegistry,
    credentials: &dyn CredentialResolver,
    materialized: &mut BTreeMap<String, Credential>,
    route: &Route,
    codec: Arc<dyn ClientCodec>,
) -> Result<Arc<AttemptChain>, LlmBuildError> {
    let primary = route.target().name();
    let mut members = Vec::with_capacity(route.fallback().len() + 1);
    for name in std::iter::once(primary).chain(route.fallback().iter().map(String::as_str)) {
        members.push(compile_member(
            document,
            providers,
            credentials,
            materialized,
            route,
            &codec,
            name,
        )?);
    }
    let chain =
        AttemptChain::try_new(members).map_err(|ChainBuildError| LlmBuildError::EmptyChain {
            route: route.id().to_owned(),
        })?;
    Ok(Arc::new(chain))
}

/// Compiles one chain member: its provider entry, adapter, credential, route facts and exchange.
///
/// `member_name` is the route's primary target when it is the route's own, otherwise a fallback
/// name; the errors name it either way, because "the second target cannot be served" is exactly what
/// an operator needs to know about a chain.
fn compile_member(
    document: &Document,
    providers: &ProviderRegistry,
    credentials: &dyn CredentialResolver,
    materialized: &mut BTreeMap<String, Credential>,
    route: &Route,
    codec: &Arc<dyn ClientCodec>,
    member_name: &str,
) -> Result<ChainMember, LlmBuildError> {
    let route_id = route.id();
    let entry =
        provider_entry(document, member_name).ok_or_else(|| LlmBuildError::MissingProvider {
            route: route_id.to_owned(),
            provider: member_name.to_owned(),
        })?;
    // The document carries an opaque token: the registry that assembles the adapters is the only
    // authority on which kinds exist, so an unknown kind is refused here per member instead of
    // while the document is parsed.
    let adapter = providers
        .provider(entry.kind())
        .ok_or_else(|| LlmBuildError::UnknownProviderKind {
            route: route_id.to_owned(),
            provider: member_name.to_owned(),
            kind: entry.kind().to_owned(),
        })?
        .clone();
    // The compiled facts must name what will actually run, so the identity reads the adapter's own
    // registry key rather than the document's token: they agree, and this one is `&'static`.
    let kind = adapter.kind();

    let credential = match materialized.get(member_name) {
        Some(credential) => credential.clone(),
        None => {
            let credential = credentials
                .resolve(entry.credential_ref())
                .map_err(|source| LlmBuildError::Credential {
                    route: route_id.to_owned(),
                    provider: member_name.to_owned(),
                    source,
                })?;
            materialized.insert(member_name.to_owned(), credential.clone());
            credential
        }
    };

    // A chain member is always a converting exchange: the data plane asks a member for a canonical
    // re-encoding, which is the convert contract. A member's own mode would be meaningless — the
    // route's mode decided that this route converts.
    let facts = Arc::new(route_facts(
        route_id,
        codec.protocol(),
        kind,
        entry,
        credential,
        ExchangeMode::Convert,
    )?);

    let exchange = Exchange::try_new(Arc::clone(codec), adapter, facts).map_err(|source| {
        LlmBuildError::MemberExchange {
            route: route_id.to_owned(),
            provider: member_name.to_owned(),
            source: Box::new(source),
        }
    })?;
    Ok(ChainMember::new(
        Arc::new(exchange),
        identity_of(entry, kind),
    ))
}

/// Builds the identity a member's long-lived state is keyed by, from its document entry.
///
/// The breaker parameters travel with the identity, so re-tuning them starts the target's state
/// over: a state that outlived its own thresholds would open or recover under rules the document no
/// longer states.
fn identity_of(entry: &Provider, kind: ProviderKind) -> TargetIdentity {
    let breaker = entry.circuit_breaker();
    TargetIdentity::new(
        entry.name(),
        entry.base_url(),
        kind,
        BreakerParams::new(breaker.failure_threshold(), breaker.cooldown_ms()),
    )
}
