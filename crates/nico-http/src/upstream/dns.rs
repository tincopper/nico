//! The DNS expansion: a publication resolves its hostname members once, and the addresses it got
//! back are the generation's members.
//!
//! A member's `address` may name a service by hostname. Left alone, every request would resolve
//! that name and use the first address back — one machine receives all the traffic, and whatever
//! the gateway observes about "the member" is a blend of every machine the name has ever pointed
//! at. So the publication resolves each hostname member once and turns each answer into its own
//! member: per-address health, per-address `least_conn`, and failover that can reach the second
//! address. The request path afterwards only ever sees concrete addresses.
//!
//! Three rules shape the expansion:
//!
//! - **A name's answers are one endpoint's members.** The member keeps the operator's name (it is
//!   the role, not the machine) and weight; the address distinguishes the machines. Traffic shares
//!   scale with the address count, the way nginx treats a resolving `server` entry.
//! - **One machine is one member.** The document model refuses two members that name the same
//!   `address` and port; expansion does not exempt a resolved collision — a hostname that resolves
//!   onto a written member, or two names pointing at one machine, is refused, because a duplicated
//!   machine would split one machine's failures across two counters.
//! - **Unresolvable is a publication refusal, not a request failure.** A hostname the resolver
//!   cannot answer fails the candidate document with its field named, and the last-known-good
//!   snapshot keeps serving. At startup the same refusal stops the process: there is no degraded
//!   form that serves a member list the resolver could not produce.
//!
//! The expansion is a publication-time fact, not a running one: the name→address snapshot changes
//! only when a new document is published. That is what keeps `DNS` off the request path — the
//! cost, and the way to update it, are the reload pipeline's.

use std::{collections::HashMap, future::Future, io, net::IpAddr, pin::Pin, sync::Arc};

use nico_core::{Document, ErrorCategory};
use thiserror::Error;

/// One upstream's hostname members: the endpoint name, and the addresses its name resolved to.
type EndpointAddresses = HashMap<Arc<str>, Vec<Arc<str>>>;

/// The addresses DNS answered for one candidate document's hostname members.
///
/// Built by [`expand_members_with`]; a publication hands it to
/// [`HttpStateHandle::publish_with_llm`](crate::HttpStateHandle::publish_with_llm) unchanged, so
/// the member list a generation selects from and the document it routes with are one publication's
/// facts. An endpoint absent from the map is a literal member: its own address is its only member.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpandedMembers {
    upstreams: HashMap<Arc<str>, EndpointAddresses>,
}

impl ExpandedMembers {
    /// Returns the addresses DNS answered for one endpoint, when the document wrote it as a
    /// hostname. `None` for a literal member: its own address is the member.
    pub(crate) fn addresses(&self, upstream: &str, endpoint: &str) -> Option<&[Arc<str>]> {
        Some(self.upstreams.get(upstream)?.get(endpoint)?.as_slice())
    }

    /// Checks the one invariant expansion must keep: a machine is one member.
    ///
    /// The document model refuses a written `(address, port)` duplicate; this is the same check
    /// over the *final* address set, where a resolved name can collide with a written member or
    /// with another name's answers.
    fn validate(&self, document: &Document) -> Result<(), MemberExpansionError> {
        for (upstream_index, upstream) in document.upstreams().iter().enumerate() {
            let per_endpoint = self.upstreams.get(upstream.name());
            let mut named: HashMap<(&str, u16), &str> = HashMap::new();
            for (endpoint_index, endpoint) in upstream.endpoints().iter().enumerate() {
                let field =
                    format!("upstreams[{upstream_index}].endpoints[{endpoint_index}].address");
                let addresses: Vec<&str> =
                    match per_endpoint.and_then(|per| per.get(endpoint.name())) {
                        Some(list) => list.iter().map(|address| address.as_ref()).collect(),
                        None => vec![endpoint.address()],
                    };
                for address in addresses {
                    let key = (address, endpoint.port());
                    if let Some(previous) = named.get(&key) {
                        return Err(MemberExpansionError::Collision {
                            field,
                            reason: format!(
                                "member `{previous}` already names {address}:{}",
                                endpoint.port()
                            ),
                        });
                    }
                    named.insert(key, endpoint.name());
                }
            }
        }
        Ok(())
    }
}

/// Why a candidate document's member addresses could not be resolved into a generation's members.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum MemberExpansionError {
    /// A hostname member resolved to no address, or the resolver did not answer.
    #[error("`{field}` could not be resolved: {reason}")]
    Unresolved {
        /// Dotted field path of the member's address, for example
        /// `upstreams[0].endpoints[1].address`.
        field: String,
        /// Why no address came back, quoted from the resolver.
        reason: String,
    },
    /// A resolved address and port name a machine another member of the same upstream names.
    #[error("`{field}` resolves to an address and port another member already names: {reason}")]
    Collision {
        /// Dotted field path of the member whose answers collide.
        field: String,
        /// The member that already names the machine, and the address and port they share.
        reason: String,
    },
}

impl MemberExpansionError {
    /// Returns the pipeline's category for this failure, so a refusal classifies itself the way
    /// every other publication stage does.
    ///
    /// An unresolved name is `Upstream`: the resolver is a dependency that did not answer, and the
    /// document may be right while the environment is not. A collision is `Client`: two members
    /// naming one machine is the document's mistake, the same bucket the document model's own
    /// duplicate-address refusal lands in.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Unresolved { .. } => ErrorCategory::Upstream,
            Self::Collision { .. } => ErrorCategory::Client,
        }
    }
}

/// One resolution: every address a name holds, or why the name cannot be answered.
pub type MemberAnswer = Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send>>;

/// The resolver a member expansion consults, boxed for storage and injection.
///
/// It answers with **every** address a name holds — the expansion turns each one into a member —
/// and with a plain [`io::Error`] when the name cannot be answered. The production value is
/// [`resolver`]; the reload pipeline's reloader carries one, so a test states the DNS facts
/// instead of depending on the environment's resolver.
pub type MemberResolver = Arc<dyn Fn(String, u16) -> MemberAnswer + Send + Sync>;

/// The production [`MemberResolver`]: [`tokio::net::lookup_host`] on Tokio's blocking pool,
/// collecting every address the name holds.
#[must_use]
pub fn resolver() -> MemberResolver {
    Arc::new(move |host: String, port: u16| {
        Box::pin(async move {
            tokio::net::lookup_host((host.as_str(), port))
                .await
                .map(|addrs| addrs.map(|addr| addr.ip()).collect())
        })
    })
}

/// Resolves the candidate document's hostname members with `resolver`.
///
/// # Errors
/// Returns [`MemberExpansionError`] when a hostname member resolves to nothing (or the resolver
/// fails), or when the resolved addresses collide with another member's address and port.
pub async fn expand_members_with(
    document: &Document,
    resolver: &MemberResolver,
) -> Result<ExpandedMembers, MemberExpansionError> {
    let mut upstreams: HashMap<Arc<str>, EndpointAddresses> = HashMap::new();
    for (upstream_index, upstream) in document.upstreams().iter().enumerate() {
        for (endpoint_index, endpoint) in upstream.endpoints().iter().enumerate() {
            // A literal stays as written: it names its machine already, and `parse` is the cheap
            // test for that — the same shortcut `lookup_host` itself takes on the request path.
            if endpoint.address().parse::<IpAddr>().is_ok() {
                continue;
            }
            let field = format!("upstreams[{upstream_index}].endpoints[{endpoint_index}].address");
            let answer = (resolver)(endpoint.address().to_owned(), endpoint.port())
                .await
                .map_err(|error| MemberExpansionError::Unresolved {
                    field: field.clone(),
                    reason: error.to_string(),
                })?;
            let mut addresses: Vec<Arc<str>> = Vec::new();
            for ip in answer {
                let address = Arc::from(ip.to_string().as_str());
                if !addresses.contains(&address) {
                    addresses.push(address);
                }
            }
            if addresses.is_empty() {
                return Err(MemberExpansionError::Unresolved {
                    field,
                    reason: "the name resolved to no address".to_owned(),
                });
            }
            upstreams
                .entry(Arc::from(upstream.name()))
                .or_default()
                .insert(Arc::from(endpoint.name()), addresses);
        }
    }
    let expanded = ExpandedMembers { upstreams };
    expanded.validate(document)?;
    Ok(expanded)
}

#[cfg(test)]
mod tests;
