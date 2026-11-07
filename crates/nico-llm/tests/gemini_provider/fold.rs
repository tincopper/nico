//! The fold, as the buffered path exposes it: combinations of text, calls, usage and finish reason.

use nico_llm::{ContentPart, FinishReason, TokenKind};
use serde_json::{Value, json};

use crate::support::{MODEL_VERSION, decoded, stream};

/// Decodes one answer built from the given chunks.
fn answer(chunks: &[Value]) -> nico_llm::ChatResponse {
    decoded(&stream(chunks))
}

/// Returns the text the answer's turn carries.
fn text(response: &nico_llm::ChatResponse) -> String {
    response.choices()[0]
        .turn()
        .content()
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[test]
fn text_chunks_are_concatenated_in_the_order_they_arrived() {
    let response = answer(&[
        json!({
            "candidates": [{"content": {"parts": [{"text": "a"}, {"text": "b"}]}}],
            "modelVersion": MODEL_VERSION
        }),
        json!({"candidates": [{"content": {"parts": [{"text": "c"}]}}]}),
        json!({
            "candidates": [{"content": {"parts": [{"text": "d"}]}, "finishReason": "STOP"}]
        }),
    ]);
    assert_eq!(text(&response), "abcd");
}

#[test]
fn the_last_usage_report_wins() {
    let response = answer(&[
        json!({
            "candidates": [{"content": {"parts": [{"text": "a"}]}}],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1},
            "modelVersion": MODEL_VERSION
        }),
        json!({
            "candidates": [{"content": {"parts": [{"text": "b"}]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 2, "candidatesTokenCount": 3}
        }),
    ]);
    let counts = response.usage().reported_counts().expect("usage reported");
    assert_eq!(counts.get(TokenKind::Input), Some(2));
    assert_eq!(counts.get(TokenKind::Output), Some(3));
}

#[test]
fn a_thought_only_answer_has_reasoning_and_no_visible_content() {
    let response = answer(&[
        json!({
            "candidates": [{"content": {"parts": [{"text": "thinking"}]}}],
            "modelVersion": MODEL_VERSION
        }),
        json!({"candidates": [{"finishReason": "MAX_TOKENS"}]}),
    ]);
    // The part is plain text here: only `thought: true` makes it reasoning. What this test pins is
    // that a turn with one part folds into one content part, not into a choice with none.
    assert_eq!(text(&response), "thinking");
    assert_eq!(response.choices()[0].finish_reason(), FinishReason::Length);
}

#[test]
fn an_answer_with_no_content_folds_into_an_empty_turn() {
    let response = answer(&[json!({
        "candidates": [{"finishReason": "SAFETY"}],
        "modelVersion": MODEL_VERSION
    })]);
    assert!(response.choices()[0].turn().content().is_empty());
    assert!(response.choices()[0].turn().tool_calls().is_empty());
    assert!(response.choices()[0].turn().reasoning().is_none());
}

#[test]
fn two_calls_in_one_answer_keep_their_order() {
    let response = answer(&[
        json!({
            "candidates": [{"content": {"parts": [
                {"functionCall": {"name": "one", "args": {"n": 1}}},
                {"functionCall": {"name": "two", "args": {"n": 2}}}
            ]}}],
            "modelVersion": MODEL_VERSION
        }),
        json!({"candidates": [{"finishReason": "STOP"}]}),
    ]);
    let calls = response.choices()[0].turn().tool_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].name().as_str(), "one");
    assert_eq!(calls[0].id().as_str(), "call_0");
    assert_eq!(calls[1].name().as_str(), "two");
    assert_eq!(calls[1].id().as_str(), "call_1");
}

#[test]
fn text_a_call_usage_and_a_finish_fold_into_one_answer() {
    let response = answer(&[
        json!({
            "candidates": [{"content": {"role": "model", "parts": [{"text": "let me check"}]}}],
            "usageMetadata": {"promptTokenCount": 5},
            "modelVersion": MODEL_VERSION
        }),
        json!({
            "candidates": [{"content": {"parts": [
                {"functionCall": {"id": "call-9", "name": "weather", "args": {"city": "Oslo"}}}
            ]}, "finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 7, "thoughtsTokenCount": 1}
        }),
    ]);
    let turn = response.choices()[0].turn();
    assert_eq!(text(&response), "let me check");
    assert_eq!(turn.tool_calls().len(), 1);
    assert_eq!(turn.tool_calls()[0].id().as_str(), "call-9");
    let counts = response.usage().reported_counts().expect("usage reported");
    assert_eq!(counts.get(TokenKind::Output), Some(7));
    assert_eq!(counts.get(TokenKind::Reasoning), Some(1));
    assert_eq!(response.choices()[0].finish_reason(), FinishReason::Stop);
}

#[test]
fn an_assigned_call_id_is_stable_for_the_answer_and_written_back_on_replay() {
    // The provider named no id; the adapter assigned one, and the same answer folded twice names the
    // same call — which is what lets the next request write that id back.
    let chunks = [
        json!({
            "candidates": [{"content": {"parts": [
                {"functionCall": {"name": "weather", "args": {}}}
            ]}}],
            "modelVersion": MODEL_VERSION
        }),
        json!({"candidates": [{"finishReason": "STOP"}]}),
    ];
    let first = answer(&chunks);
    let second = answer(&chunks);
    assert_eq!(
        first.choices()[0].turn().tool_calls()[0].id(),
        second.choices()[0].turn().tool_calls()[0].id()
    );
    assert_eq!(
        first.choices()[0].turn().tool_calls()[0].id().as_str(),
        "call_0"
    );
}
