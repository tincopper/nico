//! The `kind: wasm` plugin host, end to end: a document naming a WASM artifact is compiled at
//! publication, run in the sandbox per request, and bounded by the budget it declared.
//!
//! The scenarios share one harness shape with the other HTTP suites: a real server, a real
//! publication, and one origin that answers whatever arrived. What only these tests can prove is
//! the whole chain — the document's `kind: wasm` becomes a compiled module, the module's actions
//! drive the plugin outcomes, and a module that overruns its budget fails its request without
//! failing the process.

mod support;

use std::net::SocketAddr;

use support::{
    http::{
        RunningServer,
        client::{Request, exchange},
        free_port,
        source::TestSource,
        upstream::{MockUpstream, Reply},
        write_bootstrap,
    },
    wasm_fixtures,
};

/// The `kind: wasm` plugin entry a scenario writes: the artifact path, the ordering class, and the
/// sandbox budget the scenario varies.
fn wasm_plugin(name: &str, module_path: &str, fuel: u64) -> String {
    format!(
        "  - name: {name}\n    kind: wasm\n    config:\n      module: \"{module_path}\"\n      refuse_status: 403\n      fuel: {fuel}\n"
    )
}

/// A document whose plugin set is `plugins_text` and whose one route forwards to `upstream`.
fn document_with(plugins_text: &str, plugin_name: &str, upstream: &SocketAddr) -> String {
    format!(
        "\
apiVersion: gateway/v1
request:
  max_body_bytes: 1048576
  timeout_ms: 30000
plugins:
{plugins_text}providers: []
upstreams:
  - name: origin
    scheme: http
    host: 127.0.0.1
    port: {port}
routes:
  - id: api
    path_prefix: /api/
    upstream: origin
    plugins: [{plugin_name}]
",
        plugins_text = plugins_text,
        port = upstream.port(),
    )
}

#[tokio::test(flavor = "current_thread")]
async fn a_wasm_module_that_refuses_answers_the_document_s_refusal() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let origin = MockUpstream::start(Reply::Body {
        body: b"never".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    // The refuse fixture is compiled at test time and written where the document's `module` names it.
    let module_path = directory.path().join("refuse.wasm");
    std::fs::write(
        &module_path,
        wasm_fixtures::compiled(wasm_fixtures::REFUSE_WAT),
    )
    .expect("the fixture module writes");

    let document = document_with(
        &wasm_plugin("waf", &module_path.display().to_string(), 1_000_000),
        "waf",
        &origin.address(),
    );
    let source = TestSource::with_document(&document);
    let server =
        RunningServer::start_with_registry(directory.path(), source, nico_plugins::full_registry())
            .await;

    let response = exchange(server.address(), &Request::get("/api/items", "a.test")).await;
    assert_eq!(
        response.status(),
        403,
        "the module's refuse action carries the document's compiled refusal: {}",
        response.text()
    );
    assert_eq!(
        response.json_code().as_deref(),
        Some("plugin_refused"),
        "the stable code is the WASM kind's own, same shape as any built-in refusal"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_wasm_module_that_continues_forwards_the_request() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let origin = MockUpstream::start(Reply::Body {
        body: b"forwarded".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let module_path = directory.path().join("continue.wasm");
    std::fs::write(
        &module_path,
        wasm_fixtures::compiled(wasm_fixtures::CONTINUE_WAT),
    )
    .expect("the fixture module writes");

    let document = document_with(
        &wasm_plugin("pass", &module_path.display().to_string(), 1_000_000),
        "pass",
        &origin.address(),
    );
    let source = TestSource::with_document(&document);
    let server =
        RunningServer::start_with_registry(directory.path(), source, nico_plugins::full_registry())
            .await;

    let response = exchange(server.address(), &Request::get("/api/items", "a.test")).await;
    assert_eq!(
        response.status(),
        200,
        "the continue action forwards: {}",
        response.text()
    );
    assert_eq!(response.text(), "forwarded");
}

#[tokio::test(flavor = "current_thread")]
async fn a_module_that_overruns_its_fuel_fails_its_request_and_the_process_stays_alive() {
    let directory = tempfile::TempDir::new().expect("temporary directory");
    let _bootstrap = write_bootstrap(directory.path(), free_port(), false, 1, 1000);
    let origin = MockUpstream::start(Reply::Body {
        body: b"never".to_vec(),
        chunk: usize::MAX,
    })
    .await;

    let module_path = directory.path().join("hog.wasm");
    std::fs::write(
        &module_path,
        wasm_fixtures::compiled(wasm_fixtures::FUEL_HOG_WAT),
    )
    .expect("the fixture module writes");

    // A tiny budget: the loop cannot finish it, so the call ends as an execution failure.
    let document = document_with(
        &wasm_plugin("hog", &module_path.display().to_string(), 100_000),
        "hog",
        &origin.address(),
    );
    let source = TestSource::with_document(&document);
    let server =
        RunningServer::start_with_registry(directory.path(), source, nico_plugins::full_registry())
            .await;

    let response = exchange(server.address(), &Request::get("/api/items", "a.test")).await;
    assert_eq!(
        response.status(),
        500,
        "a sandbox overrun is an execution failure: the process answers 500"
    );
    assert_eq!(
        response.json_code().as_deref(),
        Some("internal_error"),
        "the code is the generic internal one: the detail is the log's, not the caller's"
    );

    // The process survived: the next request gets the same bounded treatment, not a dead server.
    let response = exchange(server.address(), &Request::get("/api/items", "a.test")).await;
    assert_eq!(response.status(), 500, "the gateway is still serving");
}
