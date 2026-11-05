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
}
impl HttpStateHandle {
    pub fn new() -> Arc<Self>;
    pub fn current(&self) -> Option<Arc<HttpState>>;
    // One atomic store, so no reader sees the snapshot with another document's plan.
    pub fn publish(&self, snapshot: Arc<RuntimeSnapshot>, plan: PublishedChains);
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

// nico-http: the exported peer seam a plugin uses for a target this crate cannot resolve
pub struct Destination { /* scheme, host, port */ }
impl HttpContext {
    pub fn set_destination(&mut self, destination: Destination);
    pub fn destination(&self) -> Option<&Destination>;
}

// nico-server — process assembly owns signals, runtime and shutdown
pub fn serve(bootstrap: &BootstrapConfig, source: Arc<dyn ConfigSource>, shutdown: CancellationToken) -> ExitCode;
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

### Attribution

The data plane owns the wire decisions a plugin cannot make. `HttpUpstreamRequestPolicy` strips
hop-by-hop and `Connection`-nominated request headers, and `finalize_h1_upstream_request_framing`
decides request framing. `Framing::snapshot/restore` in `proxy/forward.rs` is the single guard that
prevents a plugin from changing `content-length` or `transfer-encoding`; `PluginContext` rejects
writes to either header. A synthesized response is cleaned by `plugin/response.rs` and written by
`proxy/failure.rs::write_synthetic`. A buffered route holds each chunk back as `Some(Bytes::new())`
with `end_of_stream == false` until the stream ends, then delivers the whole body once and forces the
upstream request head to `transfer-encoding: chunked`.

## 4. Validation & Error Matrix

| Condition | Result |
|---|---|
| no snapshot published yet | `503 not_ready` |
| no route matches | `404 route_not_found` |
| a route names plugins and the published state has no chain for it | `500 internal_error` — never forwarded: a missing chain is an authentication bypass, not "no policy" |
| an authentication plugin refuses the request | `401` + `www-authenticate`, code `auth_missing` or `auth_invalid` (a document may override status/body, 4xx only) |
| a rate-limit plugin refuses the request | `429 rate_limited` + `retry-after`, rounded up to whole seconds |
| `route.target` is a provider and no plugin replaced it | `502 provider_unavailable` |
| upstream host does not resolve | `502 upstream_unreachable` |
| `scheme: https` while no TLS provider is compiled | `502 upstream_unreachable` (refused, never plaintext) |
| `Content-Length` or metered chunked body over the limit | `413 body_too_large` |
| an over-limit body in buffered mode | `413 body_too_large` |
| request body still arriving when the budget expires (caller is slow) | `408 request_timeout` |
| `ReadTimedout` / `WriteTimedout` / `ConnectTimedout` from Pingora | `504 upstream_timeout` |
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
  declares `buffers_body`.

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
| an unauthenticated request answers `401` + challenge + `x-nico-error-code`/`x-nico-trace-id`, a signed one is forwarded | `nico-server/tests/plugins_auth.rs` |
| a bucket runs out per subject, answers `429` + `retry-after`, and leaves other routes alone | `nico-server/tests/plugins_limit.rs` |
| a configured 4xx/JSON or 4xx/HTML refusal keeps the stable headers | `nico-server/tests/plugins_custom_response.rs` |
| a document whose credential cannot be read keeps the published generation serving | `nico-server/tests/plugins_config.rs` |
| publishing one generation writes the snapshot *and* its chains in one step | `nico-server/tests/publication.rs` |

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
conf.max_retries = 1;
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
| `max_retries` counts attempts (`while retries < max_retries`) | `pingora-proxy/src/lib.rs:1280` |
| `content-length` response bodies flush only on the last byte; chunked flushes per chunk — event streams must be chunked | `pingora-core/src/protocols/http/v1/body.rs:1367,1397` |
| `1xx` responses reach `response_filter`; guard with `status.is_informational()` before touching headers | `pingora-proxy/src/proxy_trait.rs:390` |
| without a TLS provider, `noop_tls::connect` returns a **pass-through** stream: `tls: true` would not encrypt | `pingora-core/src/protocols/tls/noop_tls/mod.rs` |
| `ServerConf` has no downstream read/idle timeout, so a stalled request body can only be bounded by our own deadline check in `request_body_filter` | `pingora-core/src/server/configuration/mod.rs` (field list) |
| an upstream `read_timeout` is **per read**, so a peer dribbling bytes is bounded by the deadline check in `upstream_response_body_filter`, not by the timeout | `pingora-proxy/src/lib.rs` read loop |
