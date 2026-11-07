//! The conversation as `contents[]`: folding runs, naming tool results and validating the turns.
//!
//! The canonical conversation keeps text, calls, results and reasoning as separate items while this
//! dialect carries them as turns of `parts[]`. The folding is therefore a small state machine: a run
//! of assistant items — messages, tool calls and reasoning — accumulates until another item arrives,
//! and tool results accumulate until a user turn can carry them at its head.
//!
//! Nothing is dropped. A shape this document cannot carry is refused with a typed error, never
//! flattened into visible text: a remote image has no field, a file is an undeclared capability, and a
//! tool result that is not plain text or that reports a failure names the canonical field it carries.
//! All of them answer `422`; none quotes the value.
//!
//! One gap is specific to this dialect: a `functionResponse` requires the function's **name**, and the
//! canonical result carries only the call id. The name is resolved from the `ToolCall` of the same
//! conversation, in one pass over the items before the turns are built; a result whose call is not in
//! the conversation is refused rather than written with an invented name.

use std::collections::BTreeMap;

use super::{
    super::{
        parts::{self, PartWire},
        unsupported, unsupported_field,
    },
    ContentWire,
};
use crate::{
    protocol::Capability,
    provider::ProviderError,
    schema::{ChatRequest, ContentPart, Item, Message, Role, ToolResult},
};

/// The role this dialect gives the caller's turns.
const USER: &str = "user";

/// The role this dialect gives the model's turns.
const MODEL: &str = "model";

/// The conversation split into the system instruction and the turn list.
pub(super) struct Conversation<'a> {
    pub(super) system: Option<ContentWire<'a>>,
    pub(super) contents: Vec<ContentWire<'a>>,
}

impl<'a> Conversation<'a> {
    /// Builds the turns, folding runs and naming tool results.
    pub(super) fn build(request: &'a ChatRequest) -> Result<Self, ProviderError> {
        let names = call_names(request);
        let mut system_parts = Vec::new();
        let mut contents = Vec::new();
        let mut run: Vec<PartWire<'a>> = Vec::new();
        let mut results: Vec<&'a ToolResult> = Vec::new();
        let mut spoken = false;

        for item in request.conversation() {
            match item {
                Item::Message(message) if is_system(message.role()) => {
                    // The instruction field precedes the conversation, so a later instruction has no
                    // position: it is refused rather than hoisted above turns it never governed.
                    if spoken {
                        return Err(unsupported_field("conversation[].role"));
                    }
                    push_system(&mut system_parts, message)?;
                }
                Item::Message(message) if message.role() == Role::Assistant => {
                    spoken = true;
                    flush_results(&mut contents, &mut results, &names)?;
                    parts::from_content(message.content(), &mut run)?;
                }
                Item::ToolCall(call) => {
                    spoken = true;
                    flush_results(&mut contents, &mut results, &names)?;
                    run.push(PartWire::call(call));
                }
                Item::Reasoning(reasoning) => {
                    spoken = true;
                    flush_results(&mut contents, &mut results, &names)?;
                    run.push(PartWire::thought(reasoning.text(), reasoning.signature()));
                }
                Item::Message(message) if message.role() == Role::User => {
                    spoken = true;
                    flush_run(&mut contents, &mut run);
                    let mut turn = Vec::with_capacity(results.len() + message.content().len());
                    for result in results.drain(..) {
                        turn.push(result_part(result, &names)?);
                    }
                    parts::from_content(message.content(), &mut turn)?;
                    contents.push(ContentWire {
                        role: Some(USER),
                        parts: turn,
                    });
                }
                Item::ToolResult(result) => {
                    spoken = true;
                    flush_run(&mut contents, &mut run);
                    results.push(result);
                }
                // A role this adapter does not know is never written as the caller's: the dialect has
                // two turn roles, and guessing would tell the peer the model said something it did not.
                Item::Message(_) => return Err(unsupported_field("conversation[].role")),
            }
        }
        flush_run(&mut contents, &mut run);
        flush_results(&mut contents, &mut results, &names)?;
        validate(&contents)?;
        let system = (!system_parts.is_empty()).then_some(ContentWire {
            role: None,
            parts: system_parts,
        });
        Ok(Self { system, contents })
    }
}

/// Reports whether a role belongs in the system instruction.
fn is_system(role: Role) -> bool {
    matches!(role, Role::System | Role::Developer)
}

/// Collects the tool name of every call the conversation contains, keyed by id.
///
/// The first call to name an id wins: a duplicated id is a conversation no codec produces, and the
/// first call is the one a later result answers.
fn call_names(request: &ChatRequest) -> BTreeMap<&str, &str> {
    let mut names = BTreeMap::new();
    for item in request.conversation() {
        if let Item::ToolCall(call) = item {
            names
                .entry(call.id().as_str())
                .or_insert_with(|| call.name().as_str());
        }
    }
    names
}

/// Appends one instruction turn, whose parts are text only.
fn push_system<'a>(
    parts_out: &mut Vec<PartWire<'a>>,
    message: &'a Message,
) -> Result<(), ProviderError> {
    for part in message.content() {
        match part {
            ContentPart::Text(text) => parts_out.push(PartWire::text(text.as_str())),
            // The instruction is a `parts` list like any turn, but this adapter writes text into it:
            // an image there has no position the caller asked for and is named, not dropped.
            ContentPart::Image(_) => return Err(unsupported_field("system_instruction.parts")),
            ContentPart::File { .. } => return Err(unsupported(Capability::Files)),
        }
    }
    Ok(())
}

/// Writes the model run that ended.
fn flush_run<'a>(contents: &mut Vec<ContentWire<'a>>, run: &mut Vec<PartWire<'a>>) {
    if run.is_empty() {
        return;
    }
    contents.push(ContentWire {
        role: Some(MODEL),
        parts: std::mem::take(run),
    });
}

/// Writes the tool results that were waiting for a user turn.
fn flush_results<'a>(
    contents: &mut Vec<ContentWire<'a>>,
    results: &mut Vec<&'a ToolResult>,
    names: &BTreeMap<&'a str, &'a str>,
) -> Result<(), ProviderError> {
    if results.is_empty() {
        return Ok(());
    }
    let mut parts = Vec::with_capacity(results.len());
    for result in results.drain(..) {
        parts.push(result_part(result, names)?);
    }
    contents.push(ContentWire {
        role: Some(USER),
        parts,
    });
    Ok(())
}

/// Builds one tool result's part.
fn result_part<'a>(
    result: &'a ToolResult,
    names: &BTreeMap<&'a str, &'a str>,
) -> Result<PartWire<'a>, ProviderError> {
    if result.is_error() {
        return Err(unsupported_field("tool_result.is_error"));
    }
    let name = names
        .get(result.id().as_str())
        .ok_or_else(|| unsupported_field("tool_result.id"))?;
    Ok(PartWire::response(
        result.id().as_str(),
        name,
        joined_text(result.content())?,
    ))
}

/// Joins a tool result's text parts into the one string this dialect's `response` carries.
fn joined_text(parts: &[ContentPart]) -> Result<String, ProviderError> {
    let mut texts = Vec::with_capacity(parts.len());
    for part in parts {
        match part {
            ContentPart::Text(text) => texts.push(text.as_str()),
            // `response` is a JSON object, and the canonical result may be an image; there is no
            // member that would carry one without changing what the tool answered.
            _ => return Err(unsupported_field("tool_result.content")),
        }
    }
    Ok(texts.join("\n"))
}

/// Refuses a conversation that would be an empty turn list or would not open with a user turn.
fn validate(contents: &[ContentWire<'_>]) -> Result<(), ProviderError> {
    if contents.is_empty() {
        // An instruction alone is not a conversation: the dialect requires at least one turn.
        return Err(unsupported_field("conversation"));
    }
    if contents[0].role != Some(USER) {
        return Err(unsupported_field("conversation[].role"));
    }
    Ok(())
}
