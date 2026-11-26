//! Identity admission, attribution and tenant access through the plugin contract.

use super::*;

#[test]
fn a_tenant_identity_is_atomic_and_readable_after_authentication() {
    let mut fixture = PluginFixture::started();
    let mut state = fixture.state(Phase::Request);
    state
        .set_identity_with_tenant("key-a", Some("tenant-a"))
        .expect("tenant identity is recorded");
    state
        .set_identity_with_tenant("key-b", Some("tenant-b"))
        .expect("a second identity is ignored");
    assert_eq!(state.identity(), Some("key-a"));
    assert_eq!(state.tenant(), Some("tenant-a"));
    drop(state);

    let identity = fixture.http.identity().expect("identity is stored");
    assert_eq!(identity.subject(), "key-a");
    assert_eq!(identity.tenant(), Some("tenant-a"));
}

#[test]
fn only_the_request_authentication_phase_can_write_tenant_identity() {
    for phase in [
        Phase::RequestBody,
        Phase::Response,
        Phase::ResponseBody,
        Phase::Logging,
    ] {
        let mut fixture = PluginFixture::started().responded(200);
        let mut state = fixture.state(phase);
        assert!(matches!(
            state.set_identity_with_tenant("key", Some("tenant")),
            Err(PluginError::PhaseViolation(_))
        ));
        assert_eq!(state.tenant(), None);
    }
}
