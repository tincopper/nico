//! The whole document turned into the policies one generation runs.
//!
//! Compilation is where every fallible thing about a plugin happens, once per publication: the
//! document's `kind` string is resolved through the [`PluginRegistry`], the factory that answers it
//! reads the kind's own configuration, materializes its credentials and compiles its refusal, and the
//! entries are ordered. A published generation therefore cannot fail for a configuration reason later,
//! and a document that cannot be compiled leaves the previous generation in service instead of
//! half-publishing.
//!
//! Four decisions live here and nowhere else:
//!
//! - **Phase before document order.** A kind declares its class through
//!   [`PluginFactory::phase`](nico_http::PluginFactory::phase) — authentication before a limit — and
//!   only *within* a class does the order the operator wrote apply. A document that lists the limit
//!   first would otherwise let an unauthenticated request spend another caller's quota, so the order
//!   it wrote cannot be the order that runs.
//! - **Routes that resolve to the same entries share one chain.** A route list is interned by the
//!   entries it resolves to, so a thousand routes with three policy combinations compile three chains.
//!   An entry that is declared in the document and also named by a route is one instance in both
//!   chains: a second instance would mean two pieces of runtime state for one configured plugin.
//! - **The body mode is a property of the route.** A route whose chain contains an entry with
//!   `buffers_body` is recorded as buffered; the body hook reads that decision instead of re-reading
//!   configuration per chunk.
//! - **The plan is complete or it is refused.** A route that names a plugin and has no chain in the
//!   plan is an error, never a route that runs with no policy.
//!
//! State that must outlive a publication belongs to the kind's factory, not to this type: the registry
//! the process assembles is the one object every generation is built by, so a token-bucket table kept
//! there survives a reload without the compiler holding it.

pub(crate) mod credentials;

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fmt,
    sync::Arc,
};

use nico_core::{Document, ErrorCategory, PluginSpec, Route};
use nico_http::{
    Plugin, PluginChain, PluginEntry, PluginPhase, PluginPhases, PluginRegistry, PublishedChains,
};
use serde_norway::Value;

use crate::{PluginError, builtin_registry};

/// Reports one line per compiled plugin: what it is and where it actually runs.
///
/// Scope is read from the plan's use sites, not from the entry: a declared plugin may run globally, on
/// the routes that name it, on both, or nowhere at all. One line per publication keeps "the document
/// was accepted" and "this policy is in service" the same statement.
fn log_scope(document: &Document, compiled: &[Compiled]) {
    let global: HashSet<&str> = document
        .global_plugins()
        .iter()
        .map(String::as_str)
        .collect();
    let routed: HashSet<&str> = document
        .routes()
        .iter()
        .flat_map(Route::plugins)
        .map(String::as_str)
        .collect();

    for entry in compiled {
        let scope = match (
            global.contains(entry.name.as_str()),
            routed.contains(entry.name.as_str()),
        ) {
            (true, true) => "global+route",
            (true, false) => "global",
            (false, true) => "route",
            (false, false) => "unused",
        };
        tracing::info!(
            plugin = entry.name,
            kind = entry.kind,
            scope,
            phase = phase_label(entry.phase),
            phases = ?entry.phases,
            buffers_body = entry.buffers_body,
            enabled = true,
            "plugin compiled"
        );
    }
}

/// Returns the name a kind's ordering class is logged as.
///
/// The label is this crate's: the class itself belongs to the loading seam, which has no opinion about
/// how a log record spells it.
const fn phase_label(phase: PluginPhase) -> &'static str {
    match phase {
        PluginPhase::Authenticate => "authenticate",
        PluginPhase::Limit => "limit",
        // A kind that is neither authenticates nobody and spends nothing: it runs after both classes,
        // in document order, and the label says exactly that much.
        _ => "other",
    }
}

/// One plugin instance, compiled and ready to run.
struct Compiled {
    /// The instance name: what a route references, what `authn` records and what a rate-limit key is
    /// keyed by.
    name: String,
    /// The class its kind declared, which decides where it runs in a chain.
    phase: PluginPhase,
    /// The `kind` string the entry was built from, for the compilation log.
    kind: String,
    /// The stages the plugin declares, for the compilation log and for the framing decision.
    phases: PluginPhases,
    /// Whether the entry asked for the whole request body in one call.
    ///
    /// It rides on the compiled entry because the decision it feeds is per route: a route that names
    /// this entry buffers its body, and a route that does not keeps streaming.
    buffers_body: bool,
    /// The instance itself, shared by the global chain, every route chain and every request.
    plugin: Arc<dyn Plugin>,
    /// The kind's configuration as JSON text, exactly as [`PluginContext::config`] reports it.
    ///
    /// Kept beside the plugin because a stage's context carries it, and the request path must never
    /// re-serialize a document value. It is captured once, from the value the operator wrote.
    ///
    /// [`PluginContext::config`]: nico_http::PluginContext::config
    config: Arc<str>,
}

/// Compiles documents into the chains a generation serves.
///
/// One runtime is created by the process assembly and handed to the reload pipeline, so the registry —
/// and with it the state its factories keep, such as the token-bucket tables — survives a reload while
/// the plugins are rebuilt. It is not shared with the request path: a request only reads the plan that
/// was published, never this type.
pub struct PluginRuntime {
    /// The kinds this process can load. Constructed once: a factory may keep state across
    /// publications, so rebuilding the registry on every reload would drop it.
    registry: PluginRegistry,
}

impl PluginRuntime {
    /// Creates a runtime that loads every kind this build implements.
    #[must_use]
    pub fn new() -> Self {
        Self::with_registry(builtin_registry())
    }

    /// Creates a runtime over `registry`, which is how a process adds a kind from another source.
    ///
    /// The registry is taken by value and kept: a WASM host registers its factory here, and the chain,
    /// the phases and the outcomes stay unchanged.
    #[must_use]
    pub fn with_registry(registry: PluginRegistry) -> Self {
        Self { registry }
    }

    /// Compiles `document` into the policies one generation serves.
    ///
    /// The result is one value on purpose: the global chain, the per-route chains and the buffered
    /// route set all describe the same document, and the publication that stores them is one step.
    /// Returning them separately would let a generation publish one of them from another document.
    ///
    /// # Errors
    /// - [`PluginError::UnknownKind`] when an entry names a kind no registered factory answers to.
    /// - [`PluginError::BuildFailed`] when the kind's factory cannot build the entry — an unknown
    ///   configuration field, a missing required field, an out-of-range value, a credential reference
    ///   that cannot be read, or a refusal presentation this build cannot write. The message names the
    ///   plugin and the field or reference, never a credential value.
    /// - [`PluginError::MissingChain`] when the plan would not cover a route that names a plugin.
    pub fn compile(&self, document: &Document) -> Result<PublishedChains, PluginError> {
        let mut compiled = Vec::with_capacity(document.plugins().len());
        let mut disabled = HashSet::new();
        for plugin in document.plugins() {
            match self.compile_plugin(plugin)? {
                Some(entry) => compiled.push(entry),
                // A disabled entry keeps its name so a route may still reference it, but nothing is
                // built and nothing runs. The plan records the name so a route that lists only
                // disabled plugins still gets a chain — an empty one — rather than failing closed
                // against a plan that is working exactly as configured.
                None => {
                    disabled.insert(plugin.name().to_owned());
                }
            }
        }

        let planned = plan(document, &compiled, &disabled)?;
        log_scope(document, &compiled);
        tracing::debug!(
            plugins = compiled.len(),
            routes = document.routes().len(),
            chains = planned.chains,
            buffered = planned.buffered.len(),
            rewriting_responses = planned.rewriting_responses.len(),
            "plugin plan compiled"
        );
        Ok(PublishedChains::new(
            PluginChain::new(planned.global),
            planned
                .routes
                .into_iter()
                .map(|(route, entries)| (route, PluginChain::new(entries)))
                .collect(),
            planned.buffered.into_iter().collect(),
            planned.rewriting_responses.into_iter().collect(),
        ))
    }

    /// Compiles one entry, or reports that it is deliberately not compiled.
    ///
    /// `None` is returned only for an entry the document disabled, which is the one configured
    /// no-op: a kind nothing answers to is refused rather than skipped, because a skipped
    /// authentication plugin leaves the route running fewer policies than it names.
    fn compile_plugin(&self, plugin: &PluginSpec) -> Result<Option<Compiled>, PluginError> {
        if !plugin.is_enabled() {
            // Deliberate, logged, and the only way an entry is skipped. A disabled plugin is still
            // declared, so a route that names it keeps working — it just runs nothing for it — which
            // is what makes taking a policy out of service a one-line change.
            tracing::info!(
                plugin = plugin.name(),
                kind = plugin.kind(),
                enabled = false,
                "plugin disabled; not compiled"
            );
            return Ok(None);
        }

        let Some(factory) = self.registry.factory(plugin.kind()) else {
            // `kind` is an opaque string in the document model, so this is the registry question, and
            // it must refuse rather than skip: a skipped plugin leaves the route running *fewer*
            // policies than it names, and an authentication entry is exactly where that is fatal.
            return Err(PluginError::UnknownKind {
                plugin: plugin.name().to_owned(),
                kind: plugin.kind().to_owned(),
            });
        };

        // Read before the factory runs: a value that cannot be presented as JSON refuses the entry
        // without materializing a credential for a document that will not be published.
        let config = config_text(plugin)?;
        let plugin_instance = factory
            .build(plugin)
            .map_err(|error| PluginError::BuildFailed {
                plugin: plugin.name().to_owned(),
                reason: error.reason().to_owned(),
                category: error.category(),
            })?;

        Ok(Some(Compiled {
            name: plugin.name().to_owned(),
            phase: factory.phase(),
            kind: plugin.kind().to_owned(),
            phases: plugin_instance.info().phases(),
            buffers_body: plugin.buffers_body(),
            plugin: plugin_instance,
            config,
        }))
    }
}

impl Default for PluginRuntime {
    /// Returns [`PluginRuntime::new`]: a runtime that loads every built-in kind.
    fn default() -> Self {
        Self::new()
    }
}

/// Returns one entry's configuration as JSON text, as [`PluginContext::config`] reports it.
///
/// The text comes from the document's own value rather than from the kind's typed configuration, so a
/// plugin reads what the operator wrote — including a field the kind defaults but the document omitted.
/// An entry that wrote no `config` reads as the empty mapping, not the YAML null it is stored as.
///
/// # Errors
/// Returns [`PluginError::BuildFailed`] when the value cannot be represented as JSON — a mapping key
/// that is not a string. A kind whose typed configuration accepted such a value would otherwise hand a
/// plugin a document that is not the one the operator wrote, so the plan is refused instead.
///
/// [`PluginContext::config`]: nico_http::PluginContext::config
fn config_text(plugin: &PluginSpec) -> Result<Arc<str>, PluginError> {
    match plugin.config() {
        Value::Null => Ok(Arc::from("{}")),
        value => serde_json::to_string(value)
            .map(Arc::<str>::from)
            .map_err(|error| PluginError::BuildFailed {
                plugin: plugin.name().to_owned(),
                reason: format!("its `config` cannot be represented as JSON: {error}"),
                category: ErrorCategory::Client,
            }),
    }
}

/// The chain elements one route runs, shared by every chain that resolves to the same entries.
///
/// One `Arc` per compiled entry inside one `Arc` slice: cloning a chain is one refcount bump, and an
/// entry that is both global and route-scoped keeps one element rather than a copy per chain.
type SharedEntries = Arc<[PluginEntry]>;

/// The policies one document compiles to, before they become the published value.
///
/// Entries are interned as slices of shared `Arc`s, so the global chain and the route chains are
/// slices of the same elements: an entry that runs before routing and is named by a route keeps one
/// piece of runtime state (a bucket table, say) rather than two.
struct Planned {
    /// The chain a request runs before it is routed.
    global: SharedEntries,
    /// The chain of every route that names a plugin.
    routes: HashMap<String, SharedEntries>,
    /// The routes whose request body plugins need whole.
    buffered: BTreeSet<String>,
    /// The routes whose response body a plugin rewrites.
    rewriting_responses: BTreeSet<String>,
    /// How many distinct chains the plan allocates — what interning is measured by. The global chain
    /// counts when it has filters, a route that names every entry reuses that same slice, and a
    /// document that declares no plugin allocates none.
    chains: usize,
}

/// Builds the global chain and the chain of every route that names a plugin.
///
/// Both come from one phase-ordered list of compiled entries, so "which order does this run in" is
/// decided once: the global chain authenticates before it limits, and so does every route chain,
/// whatever order the document wrote.
fn plan(
    document: &Document,
    compiled: &[Compiled],
    disabled: &HashSet<String>,
) -> Result<Planned, PluginError> {
    // A stable sort, so two entries of the same class keep the order the operator wrote. Phase order
    // is what makes "list the limit first" mean the same thing as "list it last".
    let mut ordered: Vec<&Compiled> = compiled.iter().collect();
    ordered.sort_by_key(|plugin| plugin.phase);

    let index_of: HashMap<&str, usize> = ordered
        .iter()
        .enumerate()
        .map(|(index, plugin)| (plugin.name.as_str(), index))
        .collect();
    let phases: Vec<PluginPhase> = ordered.iter().map(|plugin| plugin.phase).collect();

    // One chain element per compiled entry, built before interning so every chain that names the
    // entry shares that one element — which is what keeps one configured plugin one instance even
    // when it runs globally and on a route.
    let elements: Vec<PluginEntry> = ordered
        .iter()
        .map(|entry| PluginEntry::new(Arc::clone(&entry.plugin), Arc::clone(&entry.config)))
        .collect();

    let mut interned: HashMap<Vec<usize>, SharedEntries> = HashMap::new();
    // `global_plugins` is the global use site: the document decides there which declared plugins run
    // before routing. Declaring a plugin is not such a decision, so an entry that is not listed does
    // not run for every request. Interning first lets a route that names the same set share the slice.
    let global = order(
        "global",
        document.global_plugins(),
        &index_of,
        &phases,
        disabled,
    )?;
    // The two per-route framing decisions a global entry can force. It runs on every route, so a body
    // it needs whole, or a response body it rewrites, is a property of the whole document rather than
    // of the routes that happen to name it.
    let global_buffers = wants_whole_body(&global, &ordered);
    let global_rewrites = rewrites_response(&global, &ordered);
    let global = intern(&mut interned, &global, &elements);

    let mut routes = HashMap::new();
    let mut buffered = BTreeSet::new();
    let mut rewriting_responses = BTreeSet::new();
    for route in document.routes() {
        let key = order(route.id(), route.plugins(), &index_of, &phases, disabled)?;
        // One entry that needs the whole body puts its route into that mode: the body can only be
        // delivered once, so a per-plugin "you get chunks, you get a buffer" split is not expressible.
        // A buffered global entry does the same for every route.
        if global_buffers || wants_whole_body(&key, &ordered) {
            buffered.insert(route.id().to_owned());
        }
        // A route whose response body is rewritten cannot keep a length-framed response.
        if global_rewrites || rewrites_response(&key, &ordered) {
            rewriting_responses.insert(route.id().to_owned());
        }
        if route.plugins().is_empty() {
            // The zero-cost path: no entry, no lookup, no chain.
            continue;
        }
        routes.insert(
            route.id().to_owned(),
            intern(&mut interned, &key, &elements),
        );
    }

    Ok(Planned {
        global,
        routes,
        buffered,
        rewriting_responses,
        // A slice no entry contributes to is not a chain, so an empty plan reports none.
        chains: interned.keys().filter(|key| !key.is_empty()).count(),
    })
}

/// Returns whether any entry of a chain needs the whole request body in one call.
fn wants_whole_body(key: &[usize], ordered: &[&Compiled]) -> bool {
    key.iter().any(|index| ordered[*index].buffers_body)
}

/// Returns whether any entry of a chain declares that it rewrites the response body.
///
/// The declaration is the plugin's, read once when the document was compiled: a route listed here has
/// its upstream `content-length` removed, because the number in it would no longer describe the bytes
/// the plugin sends.
fn rewrites_response(key: &[usize], ordered: &[&Compiled]) -> bool {
    key.iter()
        .any(|index| ordered[*index].phases.contains(PluginPhases::RESPONSE_BODY))
}

/// Returns the interned element slice for `key`, building it the first time it is asked for.
///
/// The key is positions in the phase-ordered list, not the names the document wrote: two routes that
/// list the same plugins in a different order resolve to the same key, which is what makes interning
/// follow execution order rather than text.
fn intern(
    interned: &mut HashMap<Vec<usize>, SharedEntries>,
    key: &[usize],
    elements: &[PluginEntry],
) -> SharedEntries {
    if let Some(entries) = interned.get(key) {
        return Arc::clone(entries);
    }
    // The entries are cloned by reference into the slice: one `PluginEntry` per compiled plugin, so a
    // chain that names the same set twice points at the same elements instead of rebuilding them.
    let entries: SharedEntries = key
        .iter()
        .map(|index| elements[*index].clone())
        .collect::<Vec<_>>()
        .into();
    interned.insert(key.to_vec(), Arc::clone(&entries));
    entries
}

/// Returns the entries a route runs, as positions in the phase-ordered list.
///
/// A name the plan does not compile is skipped **only** when the document declared it disabled: that
/// is a configured no-op, not a missing policy. An empty result is a real chain with no plugins, so a
/// route that lists only disabled plugins still resolves.
///
/// # Errors
/// - [`PluginError::MissingChain`] when the route names a plugin that is neither compiled nor
///   declared disabled. The document model rejects a dangling reference, so this is the compiler's own
///   coverage check: it refuses the plan instead of publishing one that leaves the route unpoliced.
fn order(
    route: &str,
    names: &[String],
    index_of: &HashMap<&str, usize>,
    phases: &[PluginPhase],
    disabled: &HashSet<String>,
) -> Result<Vec<usize>, PluginError> {
    let mut indices = Vec::with_capacity(names.len());
    for name in names {
        let Some(&index) = index_of.get(name.as_str()) else {
            if disabled.contains(name) {
                continue;
            }
            return Err(PluginError::MissingChain {
                route: route.to_owned(),
                plugin: name.clone(),
            });
        };
        indices.push(index);
    }
    indices.sort_by_key(|index| phases[*index]);
    Ok(indices)
}

impl fmt::Debug for PluginRuntime {
    /// Prints which kinds can be loaded, never the factories themselves.
    ///
    /// A diagnostic needs to know whether a document's kind can be answered, not what a factory keeps
    /// inside it.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PluginRuntime")
            .field("kinds", &self.registry)
            .finish()
    }
}

// Compiled-in proof that the runtime can be shared by every reload and every request.
const _: fn() = || {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PluginRuntime>();
};

#[cfg(test)]
mod tests;
