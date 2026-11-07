//! Decoding a Messages event stream into canonical events.
//!
//! One decoder serves one response: it holds the event framing's partial frame, the response identity
//! `message_start` announced, the content blocks still open, and whether the stream has finished. A
//! shared decoder would let one response's partial frame be completed by another one's bytes, which is
//! why [`Provider::stream_decoder`](crate::Provider::stream_decoder) creates a fresh value per
//! response. The state machine lives in [`decoder`]; this module owns the event documents it reads.
//!
//! # Event order
//!
//! `message_start` announces the identity and the prompt-side usage, so it emits [`StreamEvent::Start`]
//! and then the first [`StreamEvent::Usage`]. `content_block_start`, `content_block_delta` and
//! `content_block_stop` address a block by the `index` the peer chose: the decoder records that index
//! and reuses it as the canonical event's `index`, so a client can address a parallel tool call before
//! the call has an id. `message_delta` reports usage and why generation stopped; its usage is emitted
//! before the finish event so a client reads the answer's accounting before its end. `message_stop`
//! ends the stream and every byte after it is ignored. `ping` and an event type this adapter does not
//! know are skipped: a heartbeat is not content.
//!
//! # Truncation is never completeness
//!
//! The finish reason is what makes a stream complete. `message_stop` is this protocol's terminator and
//! a body that ends without it is still complete once the reason arrived — the terminator adds no
//! fact. A body that ends before a finish reason, or a finish that arrives with a content block still
//! open, is refused: the client must not be told the model finished when nobody said it did, and a
//! block without its end event would leave a client waiting.
//!
//! A failed frame (`{"type":"error", …}`) becomes [`StreamEvent::Error`] and ends the decoder's
//! output. The provider's message is not read; only a bounded printable `error.type` is kept for the
//! log, the same value the status table would have kept from a failure body.
//!
//! # The signature of a thinking block
//!
//! A `signature_delta` closes a thinking block with the provider's signature. The canonical stream has
//! one reasoning event, `ReasoningDelta`, which carries text and no signature, and this adapter never
//! sends a thinking block back (it does not declare `reasoning`), so the signature is validated and
//! then dropped rather than being written into the answer as text. A thinking item in a *complete*
//! response keeps its signature; only the streamed spelling has nowhere to put it.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{malformed, usage::UsageWire};
use crate::{provider::ProviderError, schema::RawJson, stream::sse};

mod decoder;

pub(super) use decoder::Decoder;

/// One event of the stream.
///
/// The event kinds share one struct because the protocol tags them with a string member rather than
/// by shape. The members a kind does not use are absent, and the handler for that kind is what
/// requires the ones it needs.
#[derive(Deserialize)]
pub(super) struct EventWire {
    #[serde(rename = "type")]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) message: Option<MessageWire>,
    #[serde(default)]
    pub(super) index: Option<u32>,
    #[serde(default)]
    pub(super) content_block: Option<BlockStartWire>,
    #[serde(default)]
    pub(super) delta: Option<DeltaWire>,
    #[serde(default)]
    pub(super) usage: Option<UsageWire>,
    #[serde(default)]
    pub(super) error: Option<ErrorBodyWire>,
    // An event's unknown top-level member has no canonical event to ride on: a stream event is a
    // fragment, and inventing an extension event would be a shape no client protocol asked for.
    #[serde(flatten)]
    pub(super) _extra: BTreeMap<String, RawJson>,
}

/// The message envelope `message_start` carries.
#[derive(Deserialize)]
pub(super) struct MessageWire {
    pub(super) id: String,
    pub(super) model: String,
    #[serde(default, rename = "type")]
    pub(super) kind: Option<String>,
    #[serde(default)]
    pub(super) role: Option<String>,
    #[serde(default)]
    pub(super) usage: Option<UsageWire>,
    #[serde(flatten)]
    pub(super) _extra: BTreeMap<String, RawJson>,
}

/// The block `content_block_start` carries.
#[derive(Deserialize)]
pub(super) struct BlockStartWire {
    #[serde(rename = "type")]
    pub(super) kind: String,
    #[serde(default)]
    pub(super) text: Option<String>,
    #[serde(default)]
    pub(super) thinking: Option<String>,
    #[serde(default)]
    pub(super) signature: Option<String>,
    #[serde(default)]
    pub(super) id: Option<String>,
    #[serde(default)]
    pub(super) name: Option<String>,
    #[serde(default)]
    pub(super) input: Option<RawJson>,
    #[serde(flatten)]
    pub(super) extra: BTreeMap<String, RawJson>,
}

/// The increment `content_block_delta` carries, or the reason and sequence `message_delta` carries.
#[derive(Default, Deserialize)]
pub(super) struct DeltaWire {
    #[serde(default, rename = "type")]
    pub(super) kind: Option<String>,
    #[serde(default)]
    pub(super) text: Option<String>,
    #[serde(default)]
    pub(super) partial_json: Option<String>,
    #[serde(default)]
    pub(super) thinking: Option<String>,
    #[serde(default)]
    pub(super) signature: Option<String>,
    #[serde(default)]
    pub(super) stop_reason: Option<String>,
    #[serde(default, rename = "stop_sequence")]
    pub(super) _stop_sequence: Option<RawJson>,
    #[serde(flatten)]
    pub(super) extra: BTreeMap<String, RawJson>,
}

/// The provider's failure object inside a stream frame.
///
/// Only `type` is read. `message` is the provider's diagnostic and is never forwarded; serde ignores
/// it because this type does not declare it.
#[derive(Deserialize)]
pub(super) struct ErrorBodyWire {
    #[serde(default, rename = "type")]
    pub(super) kind: Option<RawJson>,
}

/// Wraps a framing failure as the adapter's malformed-document error.
pub(super) fn sse_failure(error: sse::SseError) -> ProviderError {
    malformed(format!(
        "the response stream is not a valid event stream: {error}"
    ))
}
