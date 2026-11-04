# Pingora Integration Guidelines

> The HTTP data plane runs on Pingora `=0.9.1`. Every statement below was verified against that
> version's source; a upgrade is a re-verification task, not a version bump.
>
> Owner: `gateway-http`. `gateway-server` composes it, `gateway-core` never sees it.

---

## 1. Scope / Trigger

Read this before touching `crates/gateway-http/src/proxy/**`, `service.rs`, or the `gateway-server`
assembly in `src/server/`. Pingora's defaults are *product decisions* (300 s drain, retries,
compression, `server` header), so any hook, feature flag or `ServerConf` field changed here changes
gateway behaviour for every request.

## 2. Signatures

```rust
// gateway-http — HTTP semantics only; it never installs a signal handler or owns the runtime
pub fn server_conf(settings: &HttpSettings) -> Arc<ServerConf>;
pub fn service(
    conf: &Arc<ServerConf>,
    snapshot: Arc<SnapshotHandle>,
    chain: FilterChain,
    settings: &HttpSettings,
) -> Service<HttpProxy<HttpApp, ()>>;

// gateway-http, crate-private: the peer seam a filter uses for a target this crate cannot resolve
pub struct Destination { /* scheme, host, port */ }
impl HttpContext {
    pub fn set_destination(&mut self, destination: Destination);
    pub fn destination(&self) -> Option<&Destination>;
}

// gateway-server — process assembly owns signals, runtime and shutdown
pub fn serve(bootstrap: &BootstrapConfig, source: Arc<dyn ConfigSource>, shutdown: CancellationToken) -> ExitCode;
// src/server/shutdown.rs, pub(super)
fn run_args(shutdown: CancellationToken) -> RunArgs;
struct CancelWatch; // impl ShutdownSignalWatch
```

The shutdown bridge lives in `gateway-server` on purpose: a data-plane library that installs a signal
handler as a side effect of being linked cannot be embedded in a test.

## 3. Contracts

| Field | Meaning | Constraint |
|---|---|---|
| `http.listen` | bind address for the data plane | non-zero port; `127.0.0.1:0` is rejected |
| `http.h2c` | accept cleartext HTTP/2 on that listener | default `false`; only `ProxyServiceBuilder::server_options` can set it |
| `http.grace_period_ms` | how long in-flight requests keep running after shutdown begins | `1..=300_000`, default 5000, rounded **up** to whole seconds |
| `http.graceful_shutdown_ms` | upper bound on the final drain | `1..=300_000`, default 30000, `>= grace_period_ms` |
| `http.threads` | worker threads per service | `1..=256`, default 1 — `ServerConf::threads` feeds Tokio's `worker_threads`, which panics on `0` |
| `ServerConf::max_retries` | **attempts**, not retries (`while retries < max_retries`) | `1` = exactly one attempt, no retry. `0` sends no request at all |

## 4. Validation & Error Matrix

| Condition | Result |
|---|---|
| no snapshot published yet | `503 not_ready` |
| no route matches | `404 route_not_found` |
| `route.target` is a provider and no filter replaced it | `502 provider_unavailable` |
| upstream host does not resolve | `502 upstream_unreachable` |
| `scheme: https` while no TLS provider is compiled | `502 upstream_unreachable` (refused, never plaintext) |
| `Content-Length` or metered chunked body over the limit | `413 body_too_large` |
| request body still arriving when the budget expires (caller is slow) | `408 request_timeout` |
| `Connection` header nominating more than 16 headers | `400 invalid_request` |
| `ReadTimedout` / `WriteTimedout` / `ConnectTimedout` from Pingora | `504 upstream_timeout` |
| other upstream-sourced Pingora error | `502 upstream_unreachable` |
| downstream read/write failure, or shutdown mid-request | no response; log-only `http.status=499`, `error.category=Cancelled` |
| a failure after the response was committed | no second response; the stream is cut and logged as cancelled |

## 5. Good / Base / Bad Cases

- **Good**: `request_filter` reads headers only; `request_body_filter` meters bytes; the upstream is
  contacted with a peer whose `connection_timeout`/`read_timeout` carry the remaining budget.
- **Base**: a route with only `path_prefix` and an `upstream`; request and response stream through
  unchunked; response headers pass through untouched.
- **Bad**: reading `Some(body)` in `request_filter`, or collecting the response body in a filter.
  Pingora does not replay a consumed body and does not un-buffer a collected response: the upstream
  hangs or memory grows with the body.

## 6. Tests Required

| Assertion point | Test |
|---|---|
| routing does not touch the body (head arrives before the first client body write) | `gateway-server/tests/http_body.rs` |
| 8 MiB both ways keeps sha256 and order | `http_body.rs` |
| SSE events arrive per write, byte-identical, no `Content-Encoding`/`Server` | `http_streaming.rs` |
| `100 Continue` keeps exactly the origin's headers | `http_interim.rs` |
| `Content-Length` and chunked over-limit both answer `413`, upstream untouched / partial | `http_limits.rs` |
| client disconnect → upstream released, `http.status=499`, no 5xx, next request healthy | `http_cancel.rs` |
| a client that trickles its body is answered `408` and logged as a client fault | `http_limits.rs` |
| configured grace period is honoured, then `serve` returns | `http_shutdown.rs` |
| `h2c` set/cleared through `ProxyServiceBuilder` | `gateway-http/src/service/tests.rs` |
| a filter stage cannot outlive the request budget | `gateway-http/src/filter/tests.rs` |
| a cancelled token becomes a graceful termination | `gateway-server/src/server/shutdown/tests.rs` |
| `max_retries == 1`, seconds rounded up and never `0`, `threads` passthrough | `service/tests.rs` |

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
