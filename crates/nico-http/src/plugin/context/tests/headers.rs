//! Header access: which stage may read, and which may write.
//!
//! A head describes the bytes that follow it, so the data plane owns framing; and a head that is
//! already on the wire can no longer be changed. Both rules are refusals, never silent no-ops.

use super::super::Phase;
use crate::{
    plugin::{PluginContext, PluginError},
    test_support::PluginFixture,
};

const REQUEST_STAGES: [Phase; 2] = [Phase::Request, Phase::RequestBody];
const RESPONSE_STAGES: [Phase; 3] = [Phase::Response, Phase::ResponseBody, Phase::Logging];

/// Unwraps a header read a stage is allowed to perform.
fn header(value: Result<Option<String>, PluginError>) -> Option<String> {
    value.expect("the stage offers this header")
}

#[test]
fn request_headers_are_available_in_the_request_stages_only() {
    for phase in REQUEST_STAGES {
        let mut fixture = PluginFixture::started();
        let state = fixture.state(phase);
        assert_eq!(
            header(state.request_header("host")).as_deref(),
            Some("example.test")
        );
    }

    // Writable only in `on_request`: the upstream head is sent before the body hook runs, so a write
    // in `on_request_body` could never reach the peer.
    let mut fixture = PluginFixture::started();
    let mut state = fixture.state(Phase::Request);
    state
        .set_request_header("x-edge", "1")
        .expect("a request header is writable in on_request");
    assert_eq!(header(state.request_header("x-edge")).as_deref(), Some("1"));

    let mut state = fixture.state(Phase::RequestBody);
    assert!(
        matches!(
            state.set_request_header("x-edge", "1"),
            Err(PluginError::PhaseViolation(_))
        ),
        "the upstream head is already sent by the time the body stage runs"
    );

    for phase in RESPONSE_STAGES {
        let mut fixture = PluginFixture::started().responded(200);
        let mut state = fixture.state(phase);
        assert!(
            matches!(
                state.request_header("host"),
                Err(PluginError::PhaseViolation(_))
            ),
            "{phase:?} must not read the request head"
        );
        assert!(
            matches!(
                state.set_request_header("x-edge", "1"),
                Err(PluginError::PhaseViolation(_))
            ),
            "{phase:?} must not rewrite the request head"
        );
        assert!(matches!(
            state.append_request_header("x-edge", "1"),
            Err(PluginError::PhaseViolation(_))
        ));
        assert!(matches!(
            state.remove_request_header("host"),
            Err(PluginError::PhaseViolation(_))
        ));
    }
}

#[test]
fn a_plugin_cannot_write_a_framing_header() {
    // `content-length` and `transfer-encoding` describe the bytes the data plane forwards: a plugin
    // write could only desynchronise the head from the body, or be reverted behind the plugin's back.
    let mut fixture = PluginFixture::started().responded(200);
    {
        let mut state = fixture.state(Phase::Request);
        for name in ["content-length", "Transfer-Encoding"] {
            assert!(
                matches!(
                    state.set_request_header(name, "1"),
                    Err(PluginError::Execute(_))
                ),
                "{name} must be refused, not silently restored"
            );
            assert!(matches!(
                state.append_request_header(name, "1"),
                Err(PluginError::Execute(_))
            ));
            assert!(matches!(
                state.remove_request_header(name),
                Err(PluginError::Execute(_))
            ));
        }
        state
            .set_request_header("x-traced", "yes")
            .expect("a non-framing header stays writable");
    }

    let mut state = fixture.state(Phase::Response);
    for name in ["content-length", "Transfer-Encoding"] {
        assert!(matches!(
            state.set_response_header(name, "1"),
            Err(PluginError::Execute(_))
        ));
        assert!(matches!(
            state.append_response_header(name, "1"),
            Err(PluginError::Execute(_))
        ));
        assert!(matches!(
            state.remove_response_header(name),
            Err(PluginError::Execute(_))
        ));
    }
    state
        .set_response_header("x-traced", "yes")
        .expect("a non-framing header stays writable");
}
