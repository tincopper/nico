//! Tests for the seam's values: what a front-end is handed, and what it may answer with.
//!
//! The rules asserted here are the ones a front-end could otherwise break silently: a head is a final
//! status with parseable headers and no framing of its own, a request view omits a value it could not
//! read as text, and no `Debug` spelling quotes a credential or a prompt.

use super::{LlmPrepared, LlmRequest, LlmResponseHead, NoLlmRoutes};
use crate::{error::HttpError, llm::LlmRoutes};
/// A request view with two headers, one of them repeated.
fn view<'a>(body: &'a [u8]) -> LlmRequest<'a> {
    LlmRequest::new(
        "POST",
        "/v1/chat/completions",
        vec![
            ("content-type", "application/json"),
            ("x-tag", "first"),
            ("x-tag", "second"),
        ],
        body,
    )
}

#[test]
fn a_request_view_answers_the_facts_a_codec_reads() {
    let request = view(b"{\"model\":\"fast\"}");

    assert_eq!(request.method(), "POST");
    assert_eq!(request.path(), "/v1/chat/completions");
    assert_eq!(request.body(), b"{\"model\":\"fast\"}");
    assert_eq!(
        request.headers().len(),
        3,
        "a repeated name keeps both values"
    );
}

#[test]
fn a_header_lookup_is_case_insensitive_and_returns_the_first_value() {
    let request = view(b"");

    assert_eq!(request.header("Content-Type"), Some("application/json"));
    assert_eq!(
        request.header("x-tag"),
        Some("first"),
        "the protocol rules that resolve a repeated header belong to the codec"
    );
    assert_eq!(request.header("authorization"), None);
}

#[test]
fn a_request_view_prints_names_and_lengths_and_never_a_value() {
    let request = LlmRequest::new(
        "POST",
        "/v1/chat/completions",
        vec![("authorization", "Bearer sk-do-not-print")],
        b"a prompt that must not appear",
    );

    let printed = format!("{request:?}");

    assert!(
        printed.contains("authorization"),
        "the names are the report"
    );
    assert!(printed.contains("body_len"), "the length is the report");
    assert!(
        !printed.contains("sk-do-not-print"),
        "a credential must have no Debug spelling: {printed}"
    );
    assert!(
        !printed.contains("a prompt"),
        "a prompt must have no Debug spelling: {printed}"
    );
}

#[test]
fn the_mode_setting_has_no_debug_spelling_of_the_body() {
    let prepared = LlmPrepared::new(b"a prompt that must not appear".to_vec(), true)
        .with_model("fast -> gpt-4o-mini");

    let printed = format!("{prepared:?}");

    assert!(printed.contains("streaming: true"));
    assert!(
        printed.contains("fast -> gpt-4o-mini"),
        "the model is reported"
    );
    assert!(!printed.contains("a prompt"), "{printed}");
    assert_eq!(prepared.model(), Some("fast -> gpt-4o-mini"));
}

#[test]
fn a_prepared_body_is_taken_once() {
    let prepared = LlmPrepared::new(b"{}".to_vec(), false);

    assert_eq!(prepared.body(), b"{}");
    assert!(!prepared.streaming());
    assert_eq!(prepared.model(), None);
    assert_eq!(prepared.into_body(), b"{}");
}

#[test]
fn a_client_head_is_a_final_status_with_parseable_headers() {
    let head = LlmResponseHead::try_new(
        200,
        vec![
            ("content-type".to_owned(), "application/json".to_owned()),
            (
                "x-nico-error-code".to_owned(),
                "unsupported_capability".to_owned(),
            ),
        ],
    )
    .expect("a 200 with two headers is a valid client head");

    assert_eq!(head.status(), 200);
    assert_eq!(head.headers().len(), 2);
}

#[test]
fn a_client_head_cannot_be_informational() {
    // An informational response never reaches a transform: the hook forwards it untouched. A
    // front-end that answered one as the final head would write an interim response with no answer
    // behind it, so the value cannot be constructed.
    for status in [100, 101, 199, 0, 600, u16::MAX] {
        let error = LlmResponseHead::try_new(status, Vec::new())
            .expect_err("only a final status is a client head");
        assert!(
            matches!(error, HttpError::Internal { .. }),
            "a front-end defect is this side's failure, got {error:?}"
        );
    }
    for status in [200, 204, 422, 502, 599] {
        assert!(
            LlmResponseHead::try_new(status, Vec::new()).is_ok(),
            "status {status} is a final one"
        );
    }
}

#[test]
fn a_client_head_rejects_a_header_a_peer_could_not_parse() {
    for (name, value) in [
        ("bad name", "value"),
        ("x-empty", "a\r\nb"),
        ("x-non-ascii", "déjà"),
    ] {
        let error = LlmResponseHead::try_new(200, vec![(name.to_owned(), value.to_owned())])
            .expect_err("the head must not be constructible");
        assert!(matches!(error, HttpError::Internal { .. }));
    }
}

#[test]
fn a_client_head_cannot_frame_its_own_message() {
    // The data plane decides the framing and the peer: a head that named either would describe bytes
    // the client does not receive in that shape.
    for name in [
        "content-length",
        "Transfer-Encoding",
        "host",
        "connection",
        "keep-alive",
        "trailer",
    ] {
        let error = LlmResponseHead::try_new(200, vec![(name.to_owned(), "1".to_owned())])
            .expect_err("the data plane owns these headers");
        assert!(
            error.to_string().contains("internal data-plane failure"),
            "the response never repeats the rule: {error}"
        );
    }
}

#[test]
fn a_client_head_prints_names_and_never_a_value() {
    let head = LlmResponseHead::try_new(
        200,
        vec![("x-diagnostic".to_owned(), "a provider secret".to_owned())],
    )
    .expect("the head is valid");

    let printed = format!("{head:?}");

    assert!(printed.contains("x-diagnostic"));
    assert!(!printed.contains("a provider secret"), "{printed}");
}

#[test]
fn a_plan_with_no_route_answers_nothing() {
    let plan = NoLlmRoutes;

    assert!(plan.route("ai").is_none());
    assert_eq!(plan.len(), 0);
    assert!(plan.is_empty());
    assert_eq!(format!("{plan:?}"), "NoLlmRoutes");
}
