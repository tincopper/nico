//! Decoding a Chat Completions request document into the canonical request.
//!
//! The class of each top-level field is decided here once: mapped onto the canonical request,
//! preserved in [`Extensions`] because this codec has no arm for it, or refused because the canonical
//! shape cannot carry what it asks for. [`RequestWire`] declares the refused fields explicitly, so
//! they can never fall through to the extension bucket and be forwarded as if they had no meaning.

use std::collections::BTreeMap;

use serde::Deserialize;

use super::{
    declaration::{ResponseFormatWire, ToolChoiceWire, ToolWire, decode_tools},
    field, invalid, message, reject_extra, schema, unsupported,
};
use crate::{
    protocol::CodecError,
    schema::{ChatRequest, Extensions, ModelId, RawJson, ResponseFormat, Sampling, ToolChoice},
};

/// Decodes one buffered request body.
pub(super) fn decode(body: &[u8]) -> Result<ChatRequest, CodecError> {
    parse::<RequestWire>(body)?.into_request()
}

/// Parses the body, reporting only the error class and the position.
///
/// The parser's own message can quote the offending value, and a request body is the caller's
/// prompt: the diagnostic says *where* the document broke, never *what* it held.
fn parse<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, CodecError> {
    let document =
        std::str::from_utf8(body).map_err(|_| invalid("the request body is not UTF-8"))?;
    serde_json::from_str(document).map_err(|error| {
        invalid(format!(
            "the request body is not a Chat Completions document ({} error at line {}, column {})",
            error_class(&error),
            error.line(),
            error.column()
        ))
    })
}

/// Names the class of a parse failure.
fn error_class(error: &serde_json::Error) -> &'static str {
    match error.classify() {
        serde_json::error::Category::Io => "i/o",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "end of input",
    }
}

/// A Chat Completions request document.
#[derive(Deserialize)]
struct RequestWire {
    model: String,
    messages: Vec<message::MessageWire>,
    #[serde(default)]
    tools: Option<Vec<ToolWire>>,
    #[serde(default)]
    tool_choice: Option<ToolChoiceWire>,
    #[serde(default)]
    temperature: Option<f64>,
    #[serde(default)]
    top_p: Option<f64>,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    max_completion_tokens: Option<u32>,
    #[serde(default)]
    stop: Option<StopWire>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    presence_penalty: Option<f64>,
    #[serde(default)]
    frequency_penalty: Option<f64>,
    #[serde(default)]
    response_format: Option<ResponseFormatWire>,
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    stream_options: Option<StreamOptionsWire>,
    // Fields this codec knows and refuses: they are declared so they can never be mistaken for an
    // unmodelled field that may be preserved. A value here that asks for nothing (`n: 1`, an empty
    // logit bias, text-only modalities) is accepted, because it names the behaviour text already has.
    #[serde(default)]
    n: Option<u32>,
    #[serde(default)]
    audio: Option<RawJson>,
    #[serde(default)]
    logprobs: Option<bool>,
    #[serde(default)]
    top_logprobs: Option<u32>,
    #[serde(default)]
    logit_bias: Option<BTreeMap<String, f64>>,
    #[serde(default)]
    modalities: Option<Vec<String>>,
    #[serde(default)]
    parallel_tool_calls: Option<bool>,
    #[serde(default)]
    functions: Option<RawJson>,
    #[serde(default)]
    function_call: Option<RawJson>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    prediction: Option<RawJson>,
    #[serde(default)]
    web_search_options: Option<RawJson>,
    #[serde(default)]
    store: Option<bool>,
    #[serde(default)]
    service_tier: Option<String>,
    #[serde(flatten)]
    extensions: Extensions,
}

impl RequestWire {
    /// Builds the canonical request, or refuses the document.
    fn into_request(self) -> Result<ChatRequest, CodecError> {
        self.reject_unsupported()?;
        let sampling = self.decode_sampling()?;
        let response_format = self.decode_response_format()?;
        let tool_choice = self.decode_tool_choice()?;

        let model = ModelId::try_new(self.model).map_err(|error| field("model", error))?;
        let conversation = message::decode(self.messages)?;
        let tools = decode_tools(self.tools)?;
        let extensions = self.extensions;

        let mut request = ChatRequest::try_new(model, conversation).map_err(schema)?;
        if !tools.is_empty() {
            request = request.with_tools(tools).map_err(schema)?;
        }
        if let Some(tool_choice) = tool_choice {
            request = request.with_tool_choice(tool_choice).map_err(schema)?;
        }
        request = request
            .with_sampling(sampling)
            .with_response_format(response_format)
            .with_stream(self.stream.unwrap_or(false))
            .with_extensions(extensions);
        request.validate().map_err(schema)?;
        Ok(request)
    }

    /// Refuses every known top-level field whose meaning the canonical request cannot carry.
    fn reject_unsupported(&self) -> Result<(), CodecError> {
        if self.n.is_some_and(|count| count != 1) {
            return Err(unsupported("n"));
        }
        if self.audio.is_some() {
            return Err(unsupported("audio"));
        }
        if self.logprobs == Some(true) {
            return Err(unsupported("logprobs"));
        }
        if self.top_logprobs.is_some_and(|count| count > 0) {
            return Err(unsupported("top_logprobs"));
        }
        if self
            .logit_bias
            .as_ref()
            .is_some_and(|bias| !bias.is_empty())
        {
            return Err(unsupported("logit_bias"));
        }
        if let Some(modalities) = &self.modalities {
            let text_only =
                modalities.len() == 1 && modalities.first().is_some_and(|kind| kind == "text");
            if !text_only {
                return Err(unsupported("modalities"));
            }
        }
        if self.parallel_tool_calls == Some(false) {
            return Err(unsupported("parallel_tool_calls"));
        }
        if self.functions.is_some() {
            return Err(unsupported("functions"));
        }
        if self.function_call.is_some() {
            return Err(unsupported("function_call"));
        }
        if self.reasoning_effort.is_some() {
            return Err(unsupported("reasoning_effort"));
        }
        if self.prediction.is_some() {
            return Err(unsupported("prediction"));
        }
        if self.web_search_options.is_some() {
            return Err(unsupported("web_search_options"));
        }
        if self.store == Some(true) {
            return Err(unsupported("store"));
        }
        if self
            .service_tier
            .as_deref()
            .is_some_and(|tier| tier != "auto")
        {
            return Err(unsupported("service_tier"));
        }
        if let Some(options) = &self.stream_options {
            reject_extra(&options.extra, "stream_options")?;
        }
        Ok(())
    }

    /// Maps the sampling knobs, or reports the one that is out of range.
    fn decode_sampling(&self) -> Result<Sampling, CodecError> {
        let mut sampling = Sampling::default();
        if let Some(value) = self.temperature {
            sampling = sampling
                .with_temperature(value)
                .map_err(|error| field("temperature", error))?;
        }
        if let Some(value) = self.top_p {
            sampling = sampling
                .with_top_p(value)
                .map_err(|error| field("top_p", error))?;
        }
        if let Some(value) = self.output_tokens()? {
            let name = if self.max_completion_tokens.is_some() {
                "max_completion_tokens"
            } else {
                "max_tokens"
            };
            sampling = sampling
                .with_max_tokens(value)
                .map_err(|error| field(name, error))?;
        }
        if let Some(stop) = &self.stop {
            sampling = sampling
                .with_stop(stop.decode())
                .map_err(|error| field("stop", error))?;
        }
        if let Some(seed) = self.seed {
            sampling = sampling.with_seed(seed);
        }
        if let Some(value) = self.presence_penalty {
            sampling = sampling
                .with_presence_penalty(value)
                .map_err(|error| field("presence_penalty", error))?;
        }
        if let Some(value) = self.frequency_penalty {
            sampling = sampling
                .with_frequency_penalty(value)
                .map_err(|error| field("frequency_penalty", error))?;
        }
        Ok(sampling)
    }

    /// Resolves the output ceiling from its two spellings.
    ///
    /// `max_completion_tokens` is the current name and `max_tokens` the older one; a client may send
    /// either. Both with the same value is accepted, both with different values is refused: silently
    /// keeping one would change the ceiling the caller asked for.
    fn output_tokens(&self) -> Result<Option<u32>, CodecError> {
        match (self.max_completion_tokens, self.max_tokens) {
            (Some(current), Some(previous)) if current != previous => Err(invalid(
                "fields `max_completion_tokens` and `max_tokens` disagree on the output ceiling",
            )),
            (Some(value), _) | (None, Some(value)) => Ok(Some(value)),
            (None, None) => Ok(None),
        }
    }

    /// Maps the response format.
    fn decode_response_format(&self) -> Result<ResponseFormat, CodecError> {
        match &self.response_format {
            Some(format) => format.decode(),
            None => Ok(ResponseFormat::Text),
        }
    }

    /// Maps how the model may choose a tool.
    fn decode_tool_choice(&self) -> Result<Option<ToolChoice>, CodecError> {
        self.tool_choice
            .as_ref()
            .map(ToolChoiceWire::decode)
            .transpose()
    }
}

/// The output ceiling as the wire spells it: a single string or a list.
#[derive(Deserialize)]
#[serde(untagged)]
enum StopWire {
    One(String),
    Many(Vec<String>),
}

impl StopWire {
    /// Returns the stop sequences.
    fn decode(&self) -> Vec<String> {
        match self {
            Self::One(sequence) => vec![sequence.clone()],
            Self::Many(sequences) => sequences.clone(),
        }
    }
}

/// The stream-only options this codec accepts.
#[derive(Deserialize)]
struct StreamOptionsWire {
    /// Accepted whether or not it is set: this codec always writes the usage it receives, so a
    /// client that asked for it is satisfied and one that did not receives one extra terminal chunk.
    #[serde(rename = "include_usage", default)]
    _include_usage: Option<bool>,
    #[serde(flatten)]
    extra: BTreeMap<String, RawJson>,
}
