//! The phase matrix: what each stage offers, and what it refuses.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use super::Phase;

mod headers;
mod identity;

use crate::{
    plugin::{LogLevel, PluginContext, PluginError},
    test_support::PluginFixture,
};

const REQUEST_STAGES: [Phase; 2] = [Phase::Request, Phase::RequestBody];
const RESPONSE_STAGES: [Phase; 3] = [Phase::Response, Phase::ResponseBody, Phase::Logging];

/// Unwraps a header read a stage is allowed to perform.
fn header(value: Result<Option<String>, PluginError>) -> Option<String> {
    value.expect("the stage offers this header")
}

#[test]
fn the_request_facts_are_the_same_in_every_stage() {
    for phase in [
        Phase::Request,
        Phase::RequestBody,
        Phase::Response,
        Phase::ResponseBody,
        Phase::Logging,
    ] {
        let mut fixture = PluginFixture::started().responded(200);
        let state = fixture.state(phase);

        assert_eq!(state.plugin_name(), "fake", "{phase:?}");
        assert_eq!(state.trace_id(), "trace-1", "{phase:?}");
        assert_eq!(state.method(), "GET", "{phase:?}");
        assert_eq!(state.uri(), b"/api/items?page=2", "{phase:?}");
        assert_eq!(state.route_id(), Some("api"), "{phase:?}");
        assert_eq!(state.target(), Some("api-backend"), "{phase:?}");
        assert_eq!(state.config(), r#"{"header":"x-api-key"}"#, "{phase:?}");
        assert!(!state.is_websocket(), "{phase:?}");
        assert!(state.client_ip_hash().is_some(), "{phase:?}");
    }
}

#[test]
fn a_target_that_is_not_utf8_is_reported_unchanged() {
    // The bytes are what the router matched, so a policy deciding on the target must see them as they
    // arrived: a lossy decode would turn `%FF` into three replacement bytes and authorize a path the
    // request does not name.
    let mut fixture = PluginFixture::started();
    let mut state = fixture.state(Phase::Request);
    state
        .set_uri(b"/api/\xff/items")
        .expect("a non-UTF-8 target is still a request target");
    assert_eq!(state.uri(), b"/api/\xff/items");
    assert!(
        std::str::from_utf8(state.uri()).is_err(),
        "the bytes are not text, and the contract says so rather than inventing text"
    );
}

#[test]
fn a_global_stage_has_no_route_and_no_identity() {
    let mut fixture = PluginFixture::unrouted();
    let state = fixture.state(Phase::Request);

    assert_eq!(state.route_id(), None);
    assert_eq!(state.target(), None);
    assert_eq!(state.identity(), None);
}

#[test]
fn set_uri_works_in_on_request_and_nowhere_else() {
    let mut fixture = PluginFixture::started();
    let mut state = fixture.state(Phase::Request);
    state
        .set_uri(b"/v2/items?page=3")
        .expect("a request target is writable in on_request");
    assert_eq!(state.uri(), b"/v2/items?page=3");
    drop(state);

    for phase in [
        Phase::RequestBody,
        Phase::Response,
        Phase::ResponseBody,
        Phase::Logging,
    ] {
        let mut fixture = PluginFixture::started().responded(200);
        let mut state = fixture.state(phase);
        assert!(
            matches!(
                state.set_uri(b"/v2/items"),
                Err(PluginError::PhaseViolation(_))
            ),
            "{phase:?} must not rewrite the target"
        );
    }

    let mut fixture = PluginFixture::started();
    let mut state = fixture.state(Phase::Request);
    assert!(
        matches!(
            state.set_uri(b"/items with space"),
            Err(PluginError::Execute(
                "the URI is not a valid request target"
            ))
        ),
        "an unparseable target is the plugin's mistake, not the phase's"
    );
}

#[test]
fn an_absent_request_header_reads_as_none_and_a_non_utf8_one_as_absent() {
    let mut fixture = PluginFixture::started();
    fixture
        .request
        .append_header("x-binary", &b"\xff\xfe"[..])
        .expect("the fixture can hold a non-UTF-8 value");
    let state = fixture.state(Phase::Request);

    assert_eq!(header(state.request_header("x-absent")), None);
    assert_eq!(
        header(state.request_header("x-binary")),
        None,
        "a mangled credential must never be what a plugin authenticates"
    );
}

#[test]
fn set_and_append_differ_in_the_number_of_values() {
    let mut fixture = PluginFixture::started();
    let mut state = fixture.state(Phase::Request);
    state
        .append_request_header("x-tier", "bronze")
        .expect("appending is allowed");
    state
        .append_request_header("x-tier", "gold")
        .expect("appending keeps the first value");
    assert_eq!(
        header(state.request_header("x-tier")).as_deref(),
        Some("bronze"),
        "a read returns the first value, not the last"
    );
    drop(state);
    assert_eq!(
        fixture.request.headers.get_all("x-tier").iter().count(),
        2,
        "append kept both values"
    );

    let mut state = fixture.state(Phase::Request);
    state
        .set_request_header("x-tier", "silver")
        .expect("setting is allowed");
    drop(state);
    let values: Vec<&str> = fixture
        .request
        .headers
        .get_all("x-tier")
        .iter()
        .map(|value| value.to_str().expect("the fixture values are ASCII"))
        .collect();
    assert_eq!(values, ["silver"], "set replaces every existing value");
}

#[test]
fn removing_an_absent_header_is_not_an_error() {
    let mut fixture = PluginFixture::started();
    let mut state = fixture.state(Phase::Request);
    state
        .remove_request_header("x-absent")
        .expect("removing nothing is fine");
    state
        .set_request_header("x-gone", "1")
        .expect("the header is set");
    state
        .remove_request_header("x-gone")
        .expect("the header is removed");
    assert_eq!(header(state.request_header("x-gone")), None);
}

#[test]
fn an_invalid_header_name_or_value_is_an_execute_error() {
    let mut fixture = PluginFixture::started();
    let mut state = fixture.state(Phase::Request);

    for name in ["bad name", "", "x:y"] {
        assert!(
            matches!(
                state.set_request_header(name, "1"),
                Err(PluginError::Execute(
                    "a header name must be an RFC 9110 token"
                ))
            ),
            "`{name}` is not a header name"
        );
    }
    for value in ["a\r\nb", "a\nb", "déjà"] {
        assert!(
            matches!(
                state.append_request_header("x-edge", value),
                Err(PluginError::Execute(
                    "a header value must be visible ASCII, space or tab"
                ))
            ),
            "`{value}` is not a header value"
        );
    }
    assert!(matches!(
        state.remove_request_header("bad name"),
        Err(PluginError::Execute(_))
    ));
}

#[test]
fn response_facts_are_available_only_after_a_response_exists() {
    for phase in RESPONSE_STAGES {
        let mut fixture = PluginFixture::started().responded(201);
        let mut state = fixture.state(phase);
        assert_eq!(state.status(), Ok(201), "{phase:?}");
        state
            .set_response_header("x-served-by", "gateway")
            .expect("a response header is writable here");
        assert_eq!(
            header(state.response_header("x-served-by")).as_deref(),
            Some("gateway"),
            "{phase:?}"
        );
    }

    for phase in REQUEST_STAGES {
        let mut fixture = PluginFixture::started();
        let mut state = fixture.state(phase);
        assert!(
            matches!(state.status(), Err(PluginError::PhaseViolation(_))),
            "{phase:?} must not read a status"
        );
        assert!(matches!(
            state.response_header("content-type"),
            Err(PluginError::PhaseViolation(_))
        ));
        assert!(matches!(
            state.set_response_header("x-served-by", "gateway"),
            Err(PluginError::PhaseViolation(_))
        ));
        assert!(matches!(
            state.append_response_header("x-served-by", "gateway"),
            Err(PluginError::PhaseViolation(_))
        ));
        assert!(matches!(
            state.remove_response_header("content-type"),
            Err(PluginError::PhaseViolation(_))
        ));
    }
}

#[test]
fn a_response_header_can_hold_several_values() {
    let mut fixture = PluginFixture::started().responded(200);
    let mut state = fixture.state(Phase::Response);
    state
        .append_response_header("set-cookie", "a=1")
        .expect("appending is allowed");
    state
        .append_response_header("set-cookie", "b=2")
        .expect("appending keeps the first value");
    drop(state);

    let cookies: Vec<String> = fixture
        .response
        .as_ref()
        .expect("the fixture has a response")
        .headers
        .get_all("set-cookie")
        .iter()
        .map(|value| {
            value
                .to_str()
                .expect("the fixture values are ASCII")
                .to_owned()
        })
        .collect();
    assert_eq!(cookies, ["a=1", "b=2"]);
}

#[test]
fn a_stage_without_a_response_refuses_response_reads() {
    let mut fixture = PluginFixture::started();
    let state = fixture.state(Phase::Response);

    assert!(
        matches!(
            state.status(),
            Err(PluginError::PhaseViolation(
                "no response is attached to this stage"
            ))
        ),
        "a driver that forgot the response must not read a status"
    );
}

#[test]
fn bodies_are_only_available_in_their_own_stage() {
    let mut fixture = PluginFixture::started();
    let mut state = fixture
        .state(Phase::RequestBody)
        .with_body(b"hello".to_vec(), true);
    assert_eq!(state.request_body().as_deref(), Ok(&b"hello"[..]));
    assert_eq!(state.body_complete(), Ok(true));
    state
        .set_request_body(b"bye".to_vec())
        .expect("the request chunk is writable here");
    assert_eq!(state.request_body().as_deref(), Ok(&b"bye"[..]));
    assert!(
        matches!(
            state.response_body(),
            Err(PluginError::PhaseViolation(
                "the response body is only available in on_response_body"
            ))
        ),
        "a request stage cannot read the response body"
    );

    let mut fixture = PluginFixture::started().responded(200);
    let mut state = fixture
        .state(Phase::ResponseBody)
        .with_body(b"chunk".to_vec(), false);
    assert_eq!(state.response_body().as_deref(), Ok(&b"chunk"[..]));
    assert_eq!(state.body_complete(), Ok(false));
    state
        .set_response_body(b"rewritten".to_vec())
        .expect("the response chunk is writable here");
    assert_eq!(state.response_body().as_deref(), Ok(&b"rewritten"[..]));

    let mut fixture = PluginFixture::started().responded(200);
    let mut state = fixture.state(Phase::Response);
    assert!(matches!(
        state.request_body(),
        Err(PluginError::PhaseViolation(_))
    ));
    assert!(matches!(
        state.set_response_body(Vec::new()),
        Err(PluginError::PhaseViolation(
            "the response body is only available in on_response_body"
        ))
    ));
    assert!(matches!(
        state.body_complete(),
        Err(PluginError::PhaseViolation(
            "body completeness is only available in a body stage"
        ))
    ));
}

#[test]
fn the_first_identity_wins_and_is_attributed_to_the_plugin() {
    let mut fixture = PluginFixture::started();
    let mut state = fixture.state(Phase::Request);
    state
        .set_identity("alice")
        .expect("the first identity is recorded");
    state
        .set_identity("bob")
        .expect("a second identity is ignored, not refused");
    assert_eq!(state.identity(), Some("alice"));
    drop(state);

    let identity = fixture
        .http
        .identity()
        .expect("the identity reached the data plane");
    assert_eq!(identity.subject(), "alice");
    assert_eq!(
        identity.source(),
        "fake",
        "the identity names the plugin instance, which is what `authn` logs"
    );
}

#[test]
fn the_client_address_is_only_ever_a_hash() {
    let mut fixture = PluginFixture::started();
    let first = fixture
        .state(Phase::Request)
        .client_ip_hash()
        .expect("the fixture recorded an address");

    let mut same = PluginFixture::started();
    let again = same
        .state(Phase::Request)
        .client_ip_hash()
        .expect("the same address hashes the same");
    assert_eq!(first, again, "bucketing must be stable within a generation");

    let mut other = PluginFixture::unrouted();
    other
        .http
        .set_client_ip(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 9)));
    let different = other
        .state(Phase::Request)
        .client_ip_hash()
        .expect("a different address hashes differently");
    assert_ne!(first, different);
}

#[test]
fn an_ipv4_mapped_address_hashes_like_its_ipv4_form() {
    let mapped = "::ffff:192.0.2.7"
        .parse::<IpAddr>()
        .expect("the address parses");
    let plain = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 7));
    assert_eq!(
        super::hash_address(mapped.to_canonical()),
        super::hash_address(plain),
        "one caller must not hold two buckets by changing how it connects"
    );
    assert_ne!(
        super::hash_address(IpAddr::V6(Ipv6Addr::LOCALHOST)),
        super::hash_address(IpAddr::V4(Ipv4Addr::LOCALHOST))
    );
}

#[test]
fn the_stream_facts_come_from_the_headers_the_peer_sent() {
    let mut fixture = PluginFixture::started();
    fixture
        .request
        .insert_header("upgrade", "WebSocket")
        .expect("the fixture can ask for an upgrade");
    fixture
        .request
        .insert_header("accept", "text/event-stream")
        .expect("the fixture can ask for a stream");
    let state = fixture.state(Phase::Request);

    assert!(state.is_websocket());
    assert!(
        state.is_sse(),
        "before a response exists, the request's own accept header says it"
    );

    let mut fixture = PluginFixture::started().responded(200);
    fixture
        .response
        .as_mut()
        .expect("the fixture has a response")
        .insert_header("content-type", "text/event-stream")
        .expect("the fixture can answer with a stream");
    let state = fixture.state(Phase::Response);
    assert!(state.is_sse());
    assert!(!state.is_websocket());
}

#[test]
fn identity_cannot_be_set_once_the_response_is_written() {
    // `authn` in the request record comes from the identity slot. A response or logging plugin that
    // could write it would make an unauthenticated request look authenticated after the fact.
    for phase in RESPONSE_STAGES {
        let mut fixture = PluginFixture::started().responded(200);
        let mut state = fixture.state(phase);

        assert!(
            matches!(
                state.set_identity("mallory"),
                Err(PluginError::PhaseViolation(_))
            ),
            "{phase:?} must be read-only for identity"
        );
        assert_eq!(state.identity(), None, "{phase:?} left no identity");
    }
}

#[test]
fn a_plugin_line_is_attributed_and_counted() {
    let mut fixture = PluginFixture::started();
    let mut state = fixture.state(Phase::Request);
    assert_eq!(state.log_lines(), 0);

    state.log(LogLevel::Info, "compiled with three keys");
    state.log(LogLevel::Error, "the key set is empty");
    assert_eq!(state.log_lines(), 2);

    // Logging is available in every stage: it is how a plugin reports what only it knows.
    let mut fixture = PluginFixture::started().responded(200);
    let mut state = fixture.state(Phase::Logging);
    state.log(LogLevel::Warn, "a credential is close to expiry");
    assert_eq!(state.log_lines(), 1);
}
