//! The block table: an index that was never opened, a reused one, and the bound on the table.

use nico_llm::{ProviderError, provider::anthropic::MAX_CONTENT_BLOCKS};
use serde_json::json;

use super::{block_stop, json_event, push_err, start, text, text_block};

#[test]
fn a_block_stop_without_its_start_is_refused() {
    assert!(matches!(
        push_err(&[start(), block_stop(0)]),
        ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_delta_without_an_open_block_is_refused() {
    assert!(matches!(
        push_err(&[start(), text(0, "hi")]),
        ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_delta_that_does_not_match_its_block_is_refused() {
    let partial = json_event(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": "{}"},
        }),
    );
    assert!(matches!(
        push_err(&[start(), text_block(0), partial]),
        ProviderError::Malformed { .. }
    ));
}

#[test]
fn a_reused_block_index_is_refused() {
    let error = push_err(&[start(), text_block(0), block_stop(0), text_block(0)]);
    assert!(
        matches!(error, ProviderError::Malformed { .. }),
        "{error:?}"
    );
}

#[test]
fn more_content_blocks_than_the_bound_is_refused() {
    // The peer picks the index of every block, so the decoder caps the table it retains.
    let mut chunks = vec![start()];
    for index in 0..=MAX_CONTENT_BLOCKS {
        chunks.push(text_block(index));
    }
    assert!(matches!(push_err(&chunks), ProviderError::Malformed { .. }));
}
