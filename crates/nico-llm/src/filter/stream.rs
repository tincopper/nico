//! One response stream driven end to end: provider bytes in, canonical events, client bytes out.

use super::ExchangeError;
use crate::{protocol::ClientStreamEncoder, provider::ProviderStream};

/// Drives one response stream: provider bytes in, canonical events, client bytes out.
///
/// The SSE framing that finds event boundaries is the provider decoder's; this type only sequences
/// decoding and client encoding, so the ordering a client sees is exactly the ordering the provider
/// produced. The decoder and the client encoder are both created per response, so a partial frame or
/// a stream id can never leak from one response into another.
pub struct StreamEncoder {
    client: Box<dyn ClientStreamEncoder>,
    decoder: Box<dyn ProviderStream>,
}

impl StreamEncoder {
    /// Creates an encoder for one response from a fresh client encoder and provider decoder.
    pub(super) fn new(
        client: Box<dyn ClientStreamEncoder>,
        decoder: Box<dyn ProviderStream>,
    ) -> Self {
        Self { client, decoder }
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
        let mut bytes = Vec::new();
        for event in &events {
            bytes.extend_from_slice(&self.client.push(event)?);
        }
        Ok(bytes)
    }

    /// Signals the end of the provider body and returns the client bytes the last frame completes.
    ///
    /// The client encoder's own terminator (an event-stream sentinel, for example) is appended after
    /// the events the provider body completed.
    ///
    /// # Errors
    /// Returns the same failures as [`StreamEncoder::push`].
    pub fn finish(&mut self) -> Result<Vec<u8>, ExchangeError> {
        let events = self.decoder.finish()?;
        let mut bytes = Vec::new();
        for event in &events {
            bytes.extend_from_slice(&self.client.push(event)?);
        }
        bytes.extend_from_slice(&self.client.finish()?);
        Ok(bytes)
    }
}
