//! The retry gate and the per-round body accounting: the two facts a retry round turns on.
//!
//! Both are asserted without a session, because both are the context's own state: which of the three
//! facts admits another attempt, and what a replay round owes the counters. The hooks that call them
//! are covered beside the response and request stages.

use std::sync::{Arc, Mutex};

use super::*;
use crate::{
    context::FailureStage,
    llm::REPLAY_BODY_LIMIT,
    test_support::{FakeLlmRoute, LlmCalls, llm_context, llm_snapshot},
};

/// The replay buffer's size as the body counter takes it, so the boundary is spelled once here.
fn replay_limit() -> usize {
    usize::try_from(REPLAY_BODY_LIMIT).expect("the fixture runs on a 64-bit target")
}

/// A context on a converting route that declared attempts.
fn retrying() -> HttpContext {
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let front_end = Arc::new(FakeLlmRoute::new(&calls).with_retries());
    llm_context(&llm_snapshot(), front_end)
}

#[test]
fn a_route_that_declared_no_attempts_never_replaces_one() {
    // The undeclared route is the phase-1 behaviour: the data plane skips the retry plumbing entirely,
    // whatever the failure was and however small the body.
    let calls = Arc::new(Mutex::new(LlmCalls::default()));
    let front_end = Arc::new(FakeLlmRoute::new(&calls));
    let mut ctx = llm_context(&llm_snapshot(), front_end);

    assert!(
        !ctx.may_replace_attempt(false, FailureStage::BeforeDispatch),
        "a route with one attempt has no walk to advance"
    );
}

#[test]
fn a_failure_before_dispatch_needs_no_replay_buffer() {
    // A connect failure never put a request byte on the wire, so the client's body is still the data
    // plane's to read: any size is replayable.
    let mut ctx = retrying();
    ctx.count_body(replay_limit() * 4);

    assert!(
        ctx.may_replace_attempt(false, FailureStage::BeforeDispatch),
        "nothing was dispatched, so nothing needs the replay buffer"
    );
}

#[test]
fn a_failure_after_dispatch_needs_the_body_to_fit_the_replay_buffer() {
    let mut ctx = retrying();
    ctx.count_body(replay_limit());
    assert!(
        ctx.may_replace_attempt(false, FailureStage::AfterDispatch),
        "the buffer holds exactly its limit"
    );

    ctx.count_body(1);
    assert!(
        !ctx.may_replace_attempt(false, FailureStage::AfterDispatch),
        "one byte past the limit is a round the next attempt would send with no body at all"
    );
}

#[test]
fn a_committed_response_never_replaces_an_attempt() {
    let mut ctx = retrying();

    assert!(
        !ctx.may_replace_attempt(true, FailureStage::BeforeDispatch),
        "a response already downstream can never be replaced"
    );
}

#[test]
fn a_cancelled_client_never_replaces_an_attempt() {
    let mut ctx = retrying();
    ctx.mark_cancelled();

    assert!(
        !ctx.may_replace_attempt(false, FailureStage::BeforeDispatch),
        "nobody is left to answer"
    );
}

#[test]
fn every_client_byte_is_counted_once_across_a_replay() {
    // Round one receives a prefix; a retry round receives the same bytes again from the framework's
    // buffer. The round boundary rewinds both the count and the accumulation, so what the request
    // reports and what the canonical is built from are the client's bytes — once.
    let mut ctx = retrying();
    ctx.count_body(1000);
    ctx.push_body(b"prefix");

    ctx.begin_attempt_body();
    assert_eq!(
        ctx.body_bytes(),
        0,
        "the replay's bytes are counted by the round that receives them, not carried over"
    );

    ctx.count_body(1000);
    ctx.push_body(b"prefix");
    ctx.count_body(500);
    ctx.push_body(b"-tail");

    assert_eq!(
        ctx.body_bytes(),
        1500,
        "the client sent 1500 bytes, however many rounds delivered them"
    );
    assert_eq!(
        ctx.take_body(),
        b"prefix-tail",
        "and the canonical is built from those bytes once, not from the prefix twice"
    );
}
