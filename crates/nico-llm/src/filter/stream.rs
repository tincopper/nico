//! One response stream driven end to end: provider bytes in, canonical events, client bytes out.

use std::sync::Arc;

use super::ExchangeError;
use crate::{
    protocol::{ClientCodec, CodecError},
    provider::ProviderStream,
    schema::StreamEvent,
};

/// Drives one response stream: provider bytes in, canonical events, client bytes out.
///
/// The SSE framing that finds event boundaries is the provider decoder's; this type only sequences
/// decoding and client encoding, so the ordering a client sees is exactly the ordering the provider
/// produced. The decoder is created per response, so a partial frame can never be completed by
/// another response's bytes.
pub struct StreamEncoder {
    codec: Arc<dyn ClientCodec>,
    decoder: Box<dyn ProviderStream>,
}

impl StreamEncoder {
    /// Creates an encoder for one response from its codec and a fresh provider decoder.
    pub(super) fn new(codec: Arc<dyn ClientCodec>, decoder: Box<dyn ProviderStream>) -> Self {
        Self { codec, decoder }
    }

    /// Feeds one arbitrary chunk of the provider body and returns the client bytes it completes.
    ///
    /// The chunk is written atomically: if one event in it fails to encode, the bytes of the events
    /// already encoded for that chunk are discarded rather than written partially, and the caller
    /// ends the stream.
    ///
    /// # Errors
    /// Returns [`ExchangeError::Provider`] when the decoder cannot read the chunk, and
    /// [`ExchangeError::Codec`] when the client codec cannot encode a completed event.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        let events = self.decoder.push(chunk)?;
        Ok(self.encode(&events)?)
    }

    /// Signals the end of the provider body and returns the client bytes the last frame completes.
    ///
    /// # Errors
    /// Returns the same failures as [`StreamEncoder::push`].
    pub fn finish(&mut self) -> Result<Vec<u8>, ExchangeError> {
        let events = self.decoder.finish()?;
        Ok(self.encode(&events)?)
    }

    fn encode(&self, events: &[StreamEvent]) -> Result<Vec<u8>, CodecError> {
        let mut bytes = Vec::new();
        for event in events {
            bytes.extend_from_slice(&self.codec.encode_stream_event(event)?);
        }
        Ok(bytes)
    }
}
