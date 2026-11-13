# Pingora Integration Guidelines

> The HTTP data plane runs on Pingora `=0.9.1`. Every statement below was verified against that
> version's source; a upgrade is a re-verification task, not a version bump.
>
> Owner: `nico-http`. `nico-server` composes it, `nico-core` never sees it.

---

## 1. Scope / Trigger

Read this before touching `crates/nico-http/src/proxy/**`, `service.rs`, or the `nico-server`
assembly in `src/server/`. Pingora's defaults are *product decisions* (300 s drain, retries,
compression, `server` header), so any hook, feature flag or `ServerConf` field changed here changes
gateway behaviour for every request.

## 2. Signatures

```rust
// nico-http — HTTP semantics only; it never installs a signal handler or owns the runtime
pub fn server_conf(settings: &HttpSettings) -> Arc<ServerConf>;
pub fn service(
    conf: &Arc<ServerConf>,
    state: Arc<HttpStateHandle>,
    settings: &HttpSettings,
) -> Service<HttpProxy<HttpApp, ()>>;

// nico-http — the one published value the request path reads
pub struct HttpState {
    /* snapshot, and the plan compiled from its document: global chain, per-route chains, buffered routes */
}
impl HttpState {
    pub fn snapshot(&self) -> &Arc<RuntimeSnapshot>;
    pub fn global(&self) -> &PluginChain;
    pub fn chains(&self) -> &RouteChains;
    pub fn buffered(&self) -> &BufferedRoutes;
    pub fn rewrites_response(&self, route_id: &str) -> bool;
    pub fn llm(&self) -> &dyn LlmRoutes;
}
impl HttpStateHandle {
    pub fn new() -> Arc<Self>;
    pub fn current(&self) -> Option<Arc<HttpState>>;
    // One atomic store, so no reader sees the snapshot with another document's plan.
    pub fn publish(&self, snapshot: Arc<RuntimeSnapshot>, plan: PublishedChains);
    pub fn publish_with_llm(
        &self,
        snapshot: Arc<RuntimeSnapshot>,
        plan: PublishedChains,
        llm: Arc<dyn LlmRoutes>,
    );
}
// Everything one publication carries, built by the plugin compiler and stored unchanged: the chain that
// runs before routing, the chain each route runs, and the two per-route framing decisions — whose request
// body is buffered whole, and whose response body a plugin rewrites.
pub struct PublishedChains { /* global, routes, buffered, rewriting_responses */ }
impl RouteChains {
    pub fn chain_for(&self, route_id: &str) -> Option<&PluginChain>;
}
impl BufferedRoutes {
    pub fn contains(&self, route_id: &str) -> bool;
}
impl ResponseRewrites {
    pub fn contains(&self, route_id: &str) -> bool;
}
// The fail-closed guard (crate-private), so the hook and its test reach the same decision.
pub(crate) fn chain_for_route(
    state: &HttpState,
    route: &Route,
) -> Result<Option<PluginChain>, HttpError>;

// nico-http — the `Plugin` / `PluginContext` / `Outcome` / `ResponseData` contract names only
// owned, framework-free values (`String`, `Vec<u8>`, `u16`, `bool`, `u64`). `PluginChain` is the
// framework-facing driver: its `run_*` signatures legitimately name Pingora header and bytes types
// (`RequestHeader`, `ResponseHeader`, `Bytes`) and the data plane's `HttpContext`.
pub trait Plugin { /* name, info, five stages; only on_response_body is synchronous */ }
pub struct PluginChain { /* Arc<[PluginEntry]>: one compiled entry per plugin instance */ }
impl PluginChain {
    pub async fn run_request(&self, ctx: &mut HttpContext, request: &mut RequestHeader)
        -> Result<Option<ResponseData>, HttpError>;
    pub async fn run_request_body(&self, ctx: &mut HttpContext, request: &mut RequestHeader,
        chunk: Vec<u8>, complete: bool) -> Result<Vec<u8>, HttpError>;
    pub async fn run_response(&self, ctx: &mut HttpContext, response: &mut ResponseHeader)
        -> Result<(), HttpError>;
    pub fn run_response_body(&self, ctx: &mut HttpContext, response: Option<&ResponseHeader>,
        chunk: Bytes, complete: bool) -> Result<Option<Bytes>, HttpError>;
    pub async fn run_logging(&self, ctx: &mut HttpContext, response: Option<&ResponseHeader>);
}

// nico-http: the exported peer seam a plugin uses for a target this crate cannot resolve.
//
// `host` is the connection target and `authority` is the name the peer is addressed by (the `Host`
// header and the TLS server name). They are equal for a destination a policy builds by hand; they
// differ once an upstream has several members, because one certificate names a service while the
// service runs on many machines. `resolve` reads `host`; `host_header` and the peer's SNI read
// `authority`. Writing the machine into `Host` would address a vhost nobody declared, and using it
// for SNI would fail a certificate issued for the service.
pub struct Destination { /* scheme, host, port, authority */ }
impl HttpContext {
    pub fn set_destination(&mut self, destination: Destination);
    pub fn destination(&self) -> Option<&Destination>;
    pub fn response_limit(&self) -> Option<u32>;
}

// nico-http::llm: the byte-level seam an LLM front-end implements. No vendor name, no canonical
// type and no `Session` crosses it — the front-end answers a destination, a path, header pairs, the
// bytes to send and a per-response translator.
pub enum LlmMode { Convert, Passthrough }
pub trait LlmRoutes: Send + Sync {
    fn route(&self, route_id: &str) -> Option<Arc<dyn LlmRoute>>;
    fn len(&self) -> usize;
}
pub trait LlmRoute: Send + Sync {
    fn mode(&self) -> LlmMode;
    fn protocol(&self) -> &str;                         // reported, never matched on
    fn provider(&self) -> &str;                         // reported, never matched on
    fn destination(&self) -> &Destination;
    fn path(&self) -> &str;
    fn headers(&self) -> &[(String, String)];
    fn prepare(&self, request: &LlmRequest<'_>) -> Result<LlmPrepared, HttpError>;
    fn transform(&self, streaming: bool, max_response_bytes: u32)
        -> Result<Box<dyn ResponseTransform>, HttpError>;
}
pub trait ResponseTransform: Send {
    fn head(&mut self, status: u16, headers: &[(String, String)]) -> Result<LlmResponseHead, HttpError>;
    fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, HttpError>;
    fn finish(&mut self) -> Result<Vec<u8>, HttpError>;
}

// nico-server — process assembly owns signals, runtime and shutdown
pub fn serve(bootstrap: &BootstrapConfig, source: Arc<dyn ConfigSource>, shutdown: CancellationToken) -> ExitCode;
// The assembly seam `serve` delegates to. The registry is an assembly input, not a config knob: which
// kinds exist is decided by whoever links the process (a WASM host adds one factory), and a test
// registers a recording kind here. It is kept for the process, so factory state (token buckets)
// survives a reload.
pub fn serve_with(bootstrap: &BootstrapConfig, source: Arc<dyn ConfigSource>, shutdown: CancellationToken, plugins: PluginRegistry) -> ExitCode;
// src/server/shutdown.rs, pub(super)
fn run_args(shutdown: CancellationToken) -> RunArgs;
struct CancelWatch; // impl ShutdownSignalWatch
```

The shutdown bridge lives in `nico-server` on purpose: a data-plane library that installs a signal
handler as a side effect of being linked cannot be embedded in a test.

**The request path reads one publication, never two.** `HttpState` carries the snapshot and the
plan compiled from that same document — the global chain, the per-route chains and the buffered route
set — and `HttpStateHandle::publish` writes all of it in one
atomic store. Reading the snapshot through `SnapshotHandle` and the chains from somewhere else would
let a request see generation *N*'s routes with generation *N*−1's policies — and a policy set that is
missing its authentication plugin is fail-open, not merely stale. `SnapshotHandle` remains the
configuration-state fact source (readiness, degradation, generation for operators and tests); it is
not part of the request path.

## 3. Contracts

| Field | Meaning | Constraint |
|---|---|---|
| `http.listen` | bind address for the data plane | non-zero port; `127.0.0.1:0` is rejected |
| `http.h2c` | accept cleartext HTTP/2 on that listener | default `false`; only `ProxyServiceBuilder::server_options` can set it |
| `http.grace_period_ms` | how long in-flight requests keep running after shutdown begins | `1..=300_000`, default 5000, rounded **up** to whole seconds |
| `http.graceful_shutdown_ms` | upper bound on the final drain | `1..=300_000`, default 30000, `>= grace_period_ms` |
| `http.threads` | worker threads per service | `1..=256`, default 1 — `ServerConf::threads` feeds Tokio's `worker_threads`, which panics on `0` |
| `ServerConf::max_retries` | **attempts**, not retries (`while retries < max_retries`) | `1` = exactly one attempt, no retry. `0` sends no request at all |

### Upstream TLS

The **rustls backend is compiled in**: `nico-http` enables `pingora/rustls`, and cargo features are
additive across the graph, so an `https` destination is a real handshake from every crate that links
the data plane. Certificate and hostname verification are always on, SNI is the destination host, and
`PeerOptions.alpn` stays `H1` (an HTTP/2 upstream needs a new decision, not a flag).

| Fact | Consequence |
|---|---|
| trust roots are **process-level**: `tls.ca_file` → `ServerConf.ca_file`, otherwise the platform store | a document cannot choose roots. `ca_file` **replaces** the platform store — it does not append, so an operator who needs both concatenates them into one file |
| Pingora builds its client connector **once**, when the service is built (`ProxyServiceBuilder::build` → `ConnectorOptions::from_server_conf`) | there is no CA hot reload: changing it is a restart, and a reload only swaps the document snapshot |
| `tls::Connector::new` is `build_connector(...).unwrap()` | an unreadable or unparseable `ca_file` would abort inside `HttpProxy::new`. `nico_http::ensure_usable` re-reads the same source at assembly time (the platform store through Pingora's own `load_native_certs`, with every entry `add`ed to a root store exactly as the connector does, or the bundle again) and turns that into a startup refusal with `FAILURE_EXIT_CODE`, before any service exists |
| rustls must see exactly **one** crypto provider | `rustls` is declared with `default-features = false, features = ["ring", …]`. Its defaults enable `aws-lc-rs`, which would compile a second provider and make `CryptoProvider::from_crate_features` ambiguous — every `ClientConfig::builder` before Pingora's own install then panics |

Three files, three jobs: `upstream/tls/policy.rs` (root source and bundle validation, no Pingora
type, no I/O), `upstream/tls/adapter.rs` (the only writer of Pingora's TLS configuration), and
`upstream/tls/mod.rs`'s `ensure_usable` (the one place that reads a source at startup).

### Attribution

The data plane owns the wire decisions a plugin cannot make. `HttpUpstreamRequestPolicy` strips
hop-by-hop and `Connection`-nominated request headers, and `finalize_h1_upstream_request_framing`
decides request framing. `Framing::snapshot/restore` in `proxy/forward.rs` is the single guard that
prevents a plugin from changing `content-length` or `transfer-encoding`; `PluginContext` rejects
writes to either header. A synthesized response is cleaned by `plugin/response.rs` and written by
`proxy/failure.rs::write_synthetic`. A buffered route holds each chunk back as `Some(Bytes::new())`
with `end_of_stream == false` until the stream ends, then delivers the whole body once and forces the
upstream request head to `transfer-encoding: chunked`. Both body hooks can also be called with a
zero-byte buffer while a chunked body's terminator or trailers are still being parsed; the driver
then skips the stage instead of handing a plugin a buffer with no bytes, and keeps the request-side
`Some(empty)` (there `None` means end-of-body, so dropping it would truncate the upload).

### A multi-member upstream: where the selection happens

An upstream may name several members (`endpoints[]`). **Selection happens in exactly one place** —
`proxy/forward.rs::destination`, where an upstream becomes a `Destination` — and nowhere else:

- `upstream/pools.rs::UpstreamState::select` asks `MemberState::available` once per attempt to build
  the candidate list, then asks the `Picker` from the workspace's own `nico-lb` crate for one
  candidate. A rule may only answer with an index it was handed, so a strategy cannot override a
  health verdict.
- The candidate filter and the probe admission are separate: filtering is a question, `admit` is the
  claim. A member a strategy skipped did not spend its half-open probe.
- `Destination::new(scheme, address, port).with_authority(host)` is the one construction that splits
  "connect here" from "answer to this name". `host_header` and the peer's SNI read `authority`;
  `resolve` reads `host`.

Pingora owns the retry loop and the connection pool; it does **not** own member selection. That is why
the state lives in `UpstreamPools` (process-level, survives a reload by member identity) and the
per-generation table travels inside the published value.

A provider pool (`routes[].providers`) is a different selection surface: it is walked inside
`nico-llm`'s `AttemptSequence`, not through `forward.rs::destination`. Both reuse the same
`nico-lb::Picker`, which is the reason the contract lives in its own crate.

### An AI route (`RouteTarget::Provider`)

An AI route's bytes belong to a client protocol and a provider, neither of which this crate knows. It
reaches the data plane through `llm.rs` and changes four of the rules above:

| Step | What happens |
|---|---|
| routing | after `bind_route`, `state::llm_for_route` looks the route id up in the published plan. A provider route with no front-end is refused `502 provider_unavailable` — never forwarded and never resolved against a guessed address |
| upstream head | `hooks::llm::apply_upstream_request` writes the front-end's path and headers over the client's. `Host` and `X-Forwarded-*` are written after it, so a route can never choose the peer. `convert` drops the caller's credential in every spelling its protocol uses (`authorization`, `x-api-key`, `x-goog-api-key`: the credential describes the caller–gateway relationship, and a converted request replaces that relationship with the route's own) and its content codings — `accept-encoding` is removed, and a `content-encoding` other than `identity` is refused `400`: the answer's head is rewritten from the front-end's own headers, so a coding negotiated here would describe bytes this data plane never decodes and arrive as a parse failure behind a committed status — then the head is framed `transfer-encoding: chunked` because the body is re-encoded; `passthrough` keeps the caller's framing, coding and credential, except that a route presenting its own credential replaces the caller's: a route that materializes a key is the gateway authenticating to that peer |
| request body | `convert` holds every chunk back like a buffered route, then calls `prepare` **once** at end of stream and writes the returned bytes as the final chunk. A request with no body still reaches it (`None` + end), so an empty document is a `400` from the codec, not a request forwarded half-encoded. `passthrough` writes the bytes as they arrive. Neither runs a plugin body stage |
| response head | `hooks::llm::install_transform` creates one transform per response and calls `head(provider_status, provider_headers)`. The returned head **replaces** the provider's: every provider header is removed (they describe bytes the caller never receives), its status and headers are written, and `x-nico-trace-id` is added. `content-length` is never carried, so the framework frames the body chunked. A response that cannot carry a body (`HEAD`, `204`/`304`, `content-length: 0`) is not translated: nothing will be decoded, and a client envelope cannot be invented. An answer that arrives before the client's body ends — a provider refusing a credential it never had to read a body for — is translated as a **buffered** answer: no event stream begins before its request finishes, and answering `500` would report the provider's own `401` as this side's defect. `passthrough` forwards the provider head untouched |
| response body | `hooks::llm::transform_response_body` drives `push` per provider chunk and `finish` at end of body, writing what they return. A returned empty vector writes nothing for that task (an event may span chunks) and is not the end of the body. No plugin body stage runs |
| plugin stages | the request stages still run (authentication and rate limiting are header decisions) and so does logging. The body and response stages do not: a policy cannot see the document it would have to describe. Publication rejects the combination; the hooks skip the stages anyway, so the guarantee does not depend on the compiler |

**The upstream head precedes the decoded body.** The endpoint is a publication fact, which is why a
converted route can write the provider's path and headers before it has a body. A body refused at end
of stream (`400`/`422`, or a codec defect) may therefore already have opened the connection and
written a head with no body. That is not a forwarded request: no prompt byte reaches the origin, and
the refusal is the only answer the caller receives. The end-to-end scenarios assert on the received
body, not on the absence of a connection.

**When the model lives in the request path.** One dialect writes the physical model (and the
operation) into the request line instead of the body. Because the head precedes the body, that model
cannot be read per request, which fixes three constraints on the route:

1. the model is **pinned at publication** — the route's `model_map` must resolve to one physical
   model, which is written into the path and recorded on the endpoint, and a request that resolved
   elsewhere is `403 model_not_allowed` instead of quietly served by the pinned one;
2. the operation is the streaming one **always** — a buffered answer is folded from the same event
   stream the streaming path reads, so one route serves both client shapes;
3. a `passthrough` route is **refused at publication** — forwarding the caller's bytes to a fixed
   target would answer a different model and operation than the ones the caller named. The refusal is
   a route fact (`routes[].mode`), so the previous generation keeps serving.

**The request record names the exchange, never its contents.** An AI route adds
`llm.protocol`/`llm.provider` (publication facts) and, once the body was decoded,
`llm.model` (`logical -> physical`) and `llm.stream`. A route that is not an AI route reports none of
them, and a passthrough route or a refusal before encoding reports only the two publication facts. No
prompt, response body, header value or credential is logged.

**Where the response ceiling applies.** `request.max_response_bytes` bounds a *buffered* answer: a
converted response that is translated whole, and the declared length of one before the head is
written. A `passthrough` route buffers nothing — its bytes go through as they arrive and are never
collected — so the ceiling does not apply to it, and a document that sets one on such a route is
setting a bound nothing can cross. This is a property of the route mode, not a gap in enforcement.

**A streaming path still bounds its state.** `request.max_response_bytes` is documented as not
applying to an event stream, because nothing holds the body whole — but that exemption is about the
*body*, not about what a decoder accumulates while reading it. Every accumulator on the streaming path
declares its own bound and enforces it as it grows: the SSE framing caps one event
(`MAX_EVENT_BYTES`), the OpenAI stream caps the tool calls one response may have open
(`MAX_TOOL_CALLS`), and a wire token retained in that state (a model name, a tool name, a call id) is
capped where the type is built (`MAX_TOKEN_BYTES`). A peer chooses both how many chunks arrive and
what they contain, so "the body is streamed" is never an argument for unbounded memory.

**Where the response ceiling can and cannot be answered.** `request.max_response_bytes` (a route may
only tighten it) is enforced twice, and the two paths answer differently because Pingora has already
written the status line by the time a body chunk arrives:

- a non-streaming answer that **declares** a `content-length` over the ceiling is refused inside
  `upstream_response_filter`, before anything is written: `502 upstream_response_too_large`;
- a **chunked** answer declares nothing, so the overrun is discovered in the body hook. The head is
  committed, so no second status exists: the transform's error ends the stream, `fail_to_proxy`
  records the request as cancelled and the client sees a truncated `200`. A `502` there would be a
  second response on a connection that already has one.

An event stream is never buffered, so the ceiling does not apply to it — no check is made against a
streaming response's declared length either.

## 4. Validation & Error Matrix

| Condition | Result |
|---|---|
| no snapshot published yet | `503 not_ready` |
| no route matches | `404 route_not_found` |
| a route names plugins and the published state has no chain for it | `500 internal_error` — never forwarded: a missing chain is an authentication bypass, not "no policy" |
| an authentication plugin refuses the request | `401` + `www-authenticate`, code `auth_missing` or `auth_invalid` (a document may override status/body, 4xx only) |
| a rate-limit plugin refuses the request | `429 rate_limited` + `retry-after`, rounded up to whole seconds |
| `route.target` is a provider and no plugin replaced it | `502 provider_unavailable` |
| `route.target` is a provider and the published plan has no front-end for the route | `502 provider_unavailable` — refused at routing, never forwarded |
| a converted route's front-end cannot decode the client body | the front-end's `HttpError`: `400 invalid_request`, `422 unsupported_capability` or `500` for its own defect |
| a non-streaming answer declares a `content-length` over `max_response_bytes` | `502 upstream_response_too_large`, before the head is written |
| a chunked answer crosses `max_response_bytes` while it arrives | no second status: the head is committed, the stream ends and the request is logged as cancelled |
| upstream host does not resolve | `502 upstream_unreachable` |
| `scheme: https` and the handshake fails (certificate not trusted, hostname mismatch, protocol error) | `502 upstream_unreachable` — refused, never a plaintext downgrade; the typed cause stays in the request record |
| a TLS handshake that spends the connect deadline (`TLSHandshakeTimedout`, or `ConnectTimedout` raised on the handshake) | `504 upstream_timeout`, the same path as every other timeout |
| `Content-Length` or metered chunked body over the limit | `413 body_too_large` |
| an over-limit body in buffered mode | `413 body_too_large` |
| request body still arriving when the budget expires (caller is slow) | `408 request_timeout` |
| `ReadTimedout` / `WriteTimedout` / `ConnectTimedout` / `TLSHandshakeTimedout` from Pingora | `504 upstream_timeout` |
| other upstream-sourced Pingora error | `502 upstream_unreachable` |
| downstream read/write failure, or shutdown mid-request | no response; log-only `http.status=499`, `error.category=Cancelled` |
| a failure after the response was committed | no second response; the stream is cut and logged as cancelled |
| a plugin answers instead of forwarding (`Outcome::Respond`, request stage only) | the synthesized status, its end-to-end headers cleaned, `content-length` recomputed, `x-nico-trace-id` added |

Every row above that answers at all is written by one of **two** functions in `proxy::failure`, and
the two never overlap: `write_respond` is the sole writer for every failure (including a plugin
refusal), and `write_synthetic` is the sole writer for a synthesized answer (`Outcome::Respond`). A
policy — including every plugin — describes a failure as `HttpError` and never touches a
`ResponseHeader`, so the refusal header
set stays closed (`content-type`, `content-length`, `x-nico-error-code`, `x-nico-trace-id`, plus
`www-authenticate` and `retry-after`) and `(status, code)` is decided in exactly one place. A
synthesized answer is not a failure, so it keeps the end-to-end headers its plugin produced minus the
ones the data plane owns. The one exception to both is the informational response, which
`upstream_response_filter` and `response_filter` deliberately leave untouched.

## 5. Good / Base / Bad Cases

- **Good**: `request_filter` reads headers only; `request_body_filter` meters bytes; the upstream is
  contacted with a peer whose `connection_timeout`/`read_timeout` carry the remaining budget.
- **Base**: a route with only `path_prefix` and an `upstream`; request and response stream through
  unchunked; response headers pass through untouched.
- **Bad**: reading `Some(body)` in `request_filter`, or collecting the response body in a stage.
  Pingora does not replay a consumed body and does not un-buffer a collected response: the upstream
  hangs or memory grows with the body. The one collection this data plane does perform is the buffered
  request mode, and it is bounded by `request.max_body_bytes` and only reachable when a document
  declares `buffers_body`. The other two are bounded the same way and for the same reason: an AI
  route's `convert` mode buffers the client body to `max_body_bytes`, and a non-streaming answer is
  held to `max_response_bytes` — a chunked answer is never collected, and a ceiling crossed after the
  head was written ends the stream rather than inventing a status.

## 6. Tests Required

| Assertion point | Test |
|---|---|
| routing does not touch the body (head arrives before the first client body write) | `nico-server/tests/http_body.rs` |
| 8 MiB both ways keeps sha256 and order | `http_body.rs` |
| SSE events arrive per write, byte-identical, no `Content-Encoding`/`Server` | `http_streaming.rs` |
| `100 Continue` keeps exactly the origin's headers | `http_interim.rs` |
| `Content-Length` and chunked over-limit both answer `413`, upstream untouched / partial | `http_limits.rs` |
| client disconnect → upstream released, `http.status=499`, no 5xx, next request healthy | `http_cancel.rs` |
| a client that trickles its body is answered `408` and logged as a client fault | `http_limits.rs` |
| configured grace period is honoured, then `serve` returns | `http_shutdown.rs` |
| `h2c` set/cleared through `ProxyServiceBuilder` | `nico-http/src/service/tests.rs` |
| a plugin stage cannot outlive the request budget, a refusal or a synthesised response short-circuits the chain, and a body stage replaces or drops its chunk | `nico-http/src/plugin/chain/tests.rs` |
| a synthesised response recomputes `content-length` and drops the refusal-only headers | `nico-http/src/proxy/failure/tests.rs` |
| a cancelled token becomes a graceful termination | `nico-server/src/server/shutdown/tests.rs` |
| `max_retries == 1`, seconds rounded up and never `0`, `threads` passthrough | `service/tests.rs` |
| a route that names plugins and has no chain is refused, a route without plugins runs none | `nico-http/src/state/tests.rs` (`chain_for_route`) |
| a provider route with no published front-end is refused, an upstream route has none | `nico-http/src/state/tests.rs` (`llm_for_route`) |
| a converted body is held back, prepared once and replaced by the front-end's bytes | `nico-http/src/proxy/tests/llm.rs` |
| a converted head replaces the provider's, drops `content-length`, and adds `x-nico-trace-id` | `nico-http/src/proxy/tests/llm/response.rs` |
| an answer that precedes the client body is translated buffered, not refused as a defect | `nico-http/src/proxy/tests/llm/response.rs` |
| a declared response length over the ceiling is `502 upstream_response_too_large`; a stream is not checked | `nico-http/src/proxy/tests/llm/response.rs` |
| a response body is translated chunk by chunk and completed at end of body | `nico-http/src/proxy/tests/llm/response.rs` |
| an OpenAI client's body is decoded, model-mapped and re-encoded; the stream arrives at the origin's rate; 429/401 become canonical answers; 413/422/502 are answered without a provider body; the record carries `llm.*` only on AI routes | `nico-server/tests/llm_convert.rs` |
| a passthrough route forwards both bodies byte for byte, keeps the caller's framing and coding, presents the route's credential when it has one, and decodes nothing | `nico-server/tests/llm_passthrough.rs` |
| an unauthenticated request answers `401` + challenge + `x-nico-error-code`/`x-nico-trace-id`, a signed one is forwarded | `nico-server/tests/plugins_auth.rs` |
| a bucket runs out per subject, answers `429` + `retry-after`, and leaves other routes alone | `nico-server/tests/plugins_limit.rs` |
| a configured 4xx/JSON or 4xx/HTML refusal keeps the stable headers | `nico-server/tests/plugins_custom_response.rs` |
| a document whose credential cannot be read keeps the published generation serving | `nico-server/tests/plugins_config.rs` |
| publishing one generation writes the snapshot *and* its chains in one step | `nico-server/tests/publication.rs` |
| a global plugin runs on a request that matches no route, and before the route scope | `nico-server/tests/plugins_global.rs` |
| a buffered route withholds the body until the upload ends, then forwards the rewritten whole | `nico-server/tests/plugins_buffered.rs` |
| a synthesized response is cleaned and written without choosing a peer | `nico-server/tests/plugins_respond.rs` |
| a response-body plugin sees one chunk per upstream write, never the whole stream | `nico-server/tests/plugins_streaming.rs` |

One server per integration-test binary: `Server::run` owns the calling thread and installs a global
runtime. A second server in the same binary tests Pingora's globals, not this crate.

## 7. Wrong vs Correct

#### Wrong

```rust
// Reads the request body before routing. Pingora cannot replay it for the upstream.
async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> { /* reads body */ }

// Writes a response from a synchronous hook: error_while_proxy cannot write.
fn error_while_proxy(&self, ...) -> Option<Box<ResponseHeader>> { /* ... */ }

// Leaves retries at the Pingora default and trusts the 300 s drain.
let conf = ServerConf::default();
```

#### Correct

```rust
// Resolve on Tokio's blocking pool, so one slow lookup cannot stall every request.
let address = tokio::net::lookup_host((host, port)).await?.next()?;

// Route on headers; meter the body only.
async fn request_filter(&self, session: &mut Session, ctx: &mut Self::CTX) -> Result<bool> { /* headers only */ }

// One writer per failed request, and it is asynchronous.
async fn fail_to_proxy(&self, session: &mut Session, e: &Error, ctx: &mut Self::CTX) -> FailToProxy { /* ... */ }

// Explicit drain window, explicit attempt budget, explicit h2c.
let mut conf = ServerConf::default();
conf.grace_period_seconds = Some(seconds(settings.grace_period()));
// The loop's fuse, not the sequence: a route that declared nothing makes one attempt because its
// walk answers "no" immediately. The value is the document model's own product — the retry-budget
// ceiling times the longest chain it validates — so the fuse and the bounds cannot drift apart.
conf.max_retries = nico_core::MAX_ATTEMPTS_PER_ROUTE;
let service = ProxyServiceBuilder::new(&conf, app)
    .server_options(HttpServerOptions { h2c, ..Default::default() })
    .build();
```

## Verified constraints (re-check on every Pingora upgrade)

| Constraint | Evidence |
|---|---|
| `h2c` is reachable only through `ProxyServiceBuilder::server_options`; `http_proxy_service()` cannot set it | `pingora-proxy/src/lib.rs` builder + `HttpProxy::server_options` |
| `RunArgs { shutdown_signal: Box<dyn ShutdownSignalWatch> }` is injectable | `pingora-core/src/server/mod.rs:145,181` |
| `GracefulTerminate` = wait `grace_period_seconds`, then drain within `graceful_shutdown_timeout_seconds` | `pingora-core/src/server/mod.rs:131-140` |
| `background_service` takes a `BackgroundService`, not a future; `Server::run` returns `()` | `pingora-core/src/services/background.rs:118` |
| `ServerConf` has no `Clone`; `Server::configuration` is `pub Arc<ServerConf>` | `pingora-core/src/server/mod.rs:224` |
| `HttpPeer::new((host, port), ..)` unwraps DNS: resolve first, then pass a `SocketAddr` | `pingora-core/src/upstreams/peer.rs:718-719` |
| `PeerOptions` names the field `connection_timeout`, and has no whole-call timeout | `pingora-core/src/upstreams/peer.rs:502` |
| `max_retries` counts attempts (`while retries < max_retries`), so it must be derived from the document's own bounds — a hand-picked number is either a truncated chain or a budget nobody enforced | `pingora-proxy/src/lib.rs:1280` |
| the attempt loop re-calls `upstream_peer` and re-clones the request header every round, and re-runs `upstream_request_filter` — so a fallback's different host/port is reachable without a second session | `pingora-proxy/src/lib.rs:410,1283`; `proxy_h1.rs:48,81-122` |
| a connect failure reaches `fail_to_connect` with `retry` **false** by default; `upstream_peer`'s own `Err` (a DNS failure) does not pass through it at all | `pingora-proxy/src/lib.rs:412,479-483`; `pingora-error/src/lib.rs:209-217` |
| a request body is replayed on a retry **only** from the internal retry buffer, `BODY_BUF_LIMIT = 64 KiB`, not configurable; above it the buffer is truncated and a body-complete round sends no body at all (`request_body_filter` is never called again) | `pingora-core/src/protocols/http/v1/common.rs:32`; `body_buffer.rs:23-33`; `proxy_h1.rs:486`; `server.rs:1217-1265` |
| `upstream_response_filter` runs before any downstream write, so a failure raised there still has zero client bytes — the only response-stage retry point | `pingora-proxy/src/proxy_h1.rs:397-436` |
| the framework drops a second response head silently and does not prevent a retried round from corrupting a committed stream, so the "response already written" gate must be ours | `pingora-core/src/protocols/http/v1/server.rs:830-837` |
| `content-length` response bodies flush only on the last byte; chunked flushes per chunk — event streams must be chunked | `pingora-core/src/protocols/http/v1/body.rs:1367,1397` |
| `1xx` responses reach `response_filter`; guard with `status.is_informational()` before touching headers | `pingora-proxy/src/proxy_trait.rs:390` |
| the rustls backend is **compiled in**, so `https` completes a real handshake; the client connector is built **once**, from `ServerConf.ca_file`, so a CA change is a restart and a reload never rebuilds it | `pingora-proxy/src/lib.rs:222`; `pingora-core/src/connectors/mod.rs:188-212` |
| `tls::Connector::new` unwraps its own build, so an unreadable or unparseable `ca_file` aborts inside `HttpProxy::new`; the assembly-time `ensure_usable` probe is what closes that failure surface | `pingora-core/src/connectors/tls/rustls/mod.rs:50`; `pingora-rustls-0.9.1/src/lib.rs:107-120` |
| rustls' default features enable `aws-lc-rs`; declaring only `ring` keeps one provider. `CryptoProvider::from_crate_features` returns `None` when the features name two providers (or none), and the `expect` in `get_default_or_install_from_crate_features` then panics | `rustls-0.23.46/src/crypto/mod.rs:243,249,265`; `pingora-rustls-0.9.1/Cargo.toml` |
| the rustls path reports a handshake failure as `TLSHandshakeFailure` with `TLS connect() failed: …, SNI: …` as its context, which is why the record can tell a name mismatch from an untrusted chain | `pingora-core/src/protocols/tls/rustls/client.rs:31,37-38` |
| `ServerConf` has no downstream read/idle timeout, so a stalled request body can only be bounded by our own deadline check in `request_body_filter` | `pingora-core/src/server/configuration/mod.rs` (field list) |
| an upstream `read_timeout` is **per read**, so a peer dribbling bytes is bounded by the deadline check in `upstream_response_body_filter`, not by the timeout | `pingora-proxy/src/lib.rs` read loop |
| `read_response_task` turns a zero-length body slice into `Some(Bytes::new())` — while a chunked body's terminating CRLF or trailers are still unread, the parser yields `BufRef::new(0, 0)` — so both body hooks can be called with an empty buffer and `end_of_stream == false`; the driver skips the stage (a plugin appending to nothing would put bytes on the wire the origin never sent) and keeps the request-side `Some(empty)`, because `None` there means end-of-body | `pingora-core/src/protocols/http/v1/body.rs:720,793`; `client.rs:498,837` |
