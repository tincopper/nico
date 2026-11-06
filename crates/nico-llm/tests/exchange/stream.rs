//! The response stream, driven through arbitrary transport chunk boundaries.
//!
//! A provider's chunk boundary is the transport's, not the protocol's, so the tests here split and
//! merge frames freely; the event sequence the client sees must not depend on them.

use nico_core::ErrorCategory;
use nico_llm::{Capabilities, Exchange, ExchangeMode};

use super::mocks::{MockCodec, MockProvider, StreamBehaviour, exchange, route};

#[test]
fn stream_events_reach_the_client_in_order_across_arbitrary_chunks() {
    let exchange = exchange(Capabilities::all(), Capabilities::all());
    let mut encoder = exchange.stream();
    let mut bytes = Vec::new();
    for chunk in [b"he".as_slice(), b"llo\nwor", b"ld\n"] {
        bytes.extend_from_slice(
            &encoder
                .push(chunk)
                .expect("an arbitrary chunk boundary is safe"),
        );
    }
    bytes.extend_from_slice(&encoder.finish().expect("the stream ends cleanly"));
    assert_eq!(
        std::str::from_utf8(&bytes).expect("the encoded stream is UTF-8"),
        "text:hello\ntext:world\nfinish:Stop\n"
    );
}

#[test]
fn a_failing_provider_stream_ends_with_its_error() {
    let exchange = Exchange::try_new(
        MockCodec::new(Capabilities::all()),
        MockProvider::new(Capabilities::all(), StreamBehaviour::FailOnPush),
        route(ExchangeMode::Convert),
    )
    .expect("the mocks pair on the route");
    let error = exchange
        .stream()
        .push(b"bytes")
        .expect_err("a malformed stream is not a client event");
    assert_eq!(error.category(), ErrorCategory::Upstream);
    assert_eq!(error.canonical().code(), "upstream_protocol_error");
}
