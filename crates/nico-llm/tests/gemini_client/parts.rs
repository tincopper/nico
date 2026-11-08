//! The conversation inside `contents[]`: how one turn's parts fold into canonical items.

use nico_llm::{CodecError, ContentPart, ImageSource, Item, Role};

use crate::support::{self, refusal};

/// A user turn folds its text and its image into one message, in part order.
#[test]
fn a_users_text_and_image_fold_into_one_message() {
    let request = support::decode(
        r#"{"contents":[{"role":"user","parts":[
            {"text":"look"},{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}},{"text":"again"}
        ]}]}"#,
    )
    .expect("a turn with text and an image decodes");
    let Item::Message(message) = &request.conversation()[0] else {
        panic!("one message");
    };
    assert_eq!(
        message.content().len(),
        3,
        "the part order is the wire order"
    );
    let ContentPart::Image(ImageSource::Base64 { media_type, data }) = &message.content()[1] else {
        panic!("an inline image in the middle");
    };
    assert_eq!(media_type, "image/png");
    assert_eq!(data, b"hello");
}

/// A model turn is the assistant's, and its thought is a reasoning item with its signature.
#[test]
fn a_model_turn_carries_reasoning_with_its_signature() {
    let request = support::decode(
        r#"{"contents":[
            {"role":"user","parts":[{"text":"hi"}]},
            {"role":"model","parts":[
                {"text":"thinking","thought":true,"thoughtSignature":"sig-1"},
                {"text":"answer"}
            ]}
        ]}"#,
    )
    .expect("a thought part decodes");
    assert_eq!(
        request.conversation().len(),
        3,
        "reasoning, then the message"
    );
    let Item::Reasoning(reasoning) = &request.conversation()[1] else {
        panic!("a reasoning item second");
    };
    assert_eq!(reasoning.text(), "thinking");
    assert_eq!(reasoning.signature(), Some("sig-1"));
    let Item::Message(message) = &request.conversation()[2] else {
        panic!("a message last");
    };
    assert_eq!(message.role(), Role::Assistant);

    let error =
        refusal(r#"{"contents":[{"role":"user","parts":[{"text":"thinking","thought":true}]}]}"#);
    assert!(
        matches!(error, CodecError::InvalidRequest { .. }),
        "a thought is the model's, got {error:?}"
    );
}

/// A call without an id gets one by position; a result pairs with the call it answers.
#[test]
fn call_ids_are_assigned_by_position_and_results_pair_with_them() {
    let request = support::decode(
        r#"{"contents":[
            {"role":"user","parts":[{"text":"hi"}]},
            {"role":"model","parts":[{"functionCall":{"name":"lookup","args":{"q":"x"}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"lookup","response":{"content":"found"}}}]}
        ]}"#,
    )
    .expect("calls and results without ids decode");
    let Item::ToolCall(call) = &request.conversation()[1] else {
        panic!("a call item");
    };
    assert_eq!(
        call.id().as_str(),
        "call_0",
        "the position among the request's calls"
    );
    let Item::ToolResult(result) = &request.conversation()[2] else {
        panic!("a result item");
    };
    assert_eq!(
        result.id().as_str(),
        "call_0",
        "resolved from the same-named call"
    );
    let ContentPart::Text(text) = &result.content()[0] else {
        panic!("a text part");
    };
    assert_eq!(text, "found");

    let two_calls = support::decode(
        r#"{"contents":[
            {"role":"user","parts":[{"text":"hi"}]},
            {"role":"model","parts":[
                {"functionCall":{"name":"a","args":{}}},
                {"functionCall":{"name":"b","args":{}}}
            ]},
            {"role":"user","parts":[{"functionResponse":{"name":"a","response":{"content":"ok"}}}]}
        ]}"#,
    )
    .expect("two calls decode with positional ids");
    let Item::ToolCall(first) = &two_calls.conversation()[1] else {
        panic!("a call item");
    };
    assert_eq!(first.id().as_str(), "call_0");
    let Item::ToolResult(result) = &two_calls.conversation()[3] else {
        panic!("a result item");
    };
    assert_eq!(
        result.id().as_str(),
        "call_0",
        "the name picks the right call"
    );
}

/// A result that answers nothing, or is ambiguous, is refused rather than paired by guesswork.
#[test]
fn an_unresolvable_result_is_refused() {
    let orphan = refusal(
        r#"{"contents":[
            {"role":"user","parts":[{"functionResponse":{"name":"lookup","response":{"content":"found"}}}]}
        ]}"#,
    );
    assert!(
        matches!(orphan, CodecError::InvalidRequest { .. }),
        "no call of that name is in the conversation, got {orphan:?}"
    );

    let ambiguous = refusal(
        r#"{"contents":[
            {"role":"user","parts":[{"text":"hi"}]},
            {"role":"model","parts":[{"functionCall":{"name":"a","args":{}}},{"functionCall":{"name":"a","args":{}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"a","response":{"content":"ok"}}}]}
        ]}"#,
    );
    assert!(
        matches!(ambiguous, CodecError::InvalidRequest { .. }),
        "two same-named calls cannot be told apart, got {ambiguous:?}"
    );

    let malformed = refusal(
        r#"{"contents":[{"role":"user","parts":[{"functionResponse":{"name":"a","response":{"payload":1}}}]}]}"#,
    );
    assert!(
        matches!(malformed, CodecError::InvalidRequest { .. }),
        "the response object carries `content` and nothing else, got {malformed:?}"
    );
}

/// A `fileData` part references a stored file, which the canonical model cannot carry.
#[test]
fn a_file_reference_is_refused_at_the_capability_gate() {
    let error = refusal(
        r#"{"contents":[{"role":"user","parts":[{"fileData":{"mimeType":"application/pdf","fileUri":"gs://bucket/f.pdf"}}]}]}"#,
    );
    assert!(
        matches!(
            error,
            CodecError::Unsupported {
                feature: nico_llm::Capability::Files,
                ..
            }
        ),
        "got {error:?}"
    );
}

/// A part that mixes kinds, or carries nothing readable, is a bad document.
#[test]
fn a_part_that_mixes_kinds_or_carries_nothing_is_refused() {
    let mixed = refusal(
        r#"{"contents":[{"role":"user","parts":[{"text":"hi","inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]}]}"#,
    );
    assert!(
        matches!(mixed, CodecError::InvalidRequest { .. }),
        "got {mixed:?}"
    );

    let empty = refusal(r#"{"contents":[{"role":"user","parts":[{}]}]}"#);
    assert!(
        matches!(empty, CodecError::InvalidRequest { .. }),
        "got {empty:?}"
    );

    let bad_base64 = refusal(
        r#"{"contents":[{"role":"user","parts":[{"inlineData":{"mimeType":"image/png","data":"not*base64"}}]}]}"#,
    );
    assert!(
        matches!(bad_base64, CodecError::InvalidRequest { .. }),
        "got {bad_base64:?}"
    );
}

/// An image in a user turn decodes; the image part is a full canonical part.
#[test]
fn a_text_part_keeps_its_text_verbatim() {
    let request =
        support::decode(r#"{"contents":[{"role":"user","parts":[{"text":"hello world"}]}]}"#)
            .expect("a text part decodes");
    let Item::Message(message) = &request.conversation()[0] else {
        panic!("one message");
    };
    assert_eq!(
        message.content()[0],
        ContentPart::text("hello world"),
        "the text is not joined, trimmed or re-spelled"
    );
}
