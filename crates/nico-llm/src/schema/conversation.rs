//! Conversation items: messages, tool calls and results, and reasoning.
//!
//! A conversation is a sequence of [`Item`]s, not a vendor `messages[]` array. The difference
//! matters: the target protocols all express tool calls and tool results differently (an array on a
//! message, a content block, an input item), and a model that starts from one vendor's array ends up
//! encoding the others as strings. Here a tool call and a tool result are first-class items with
//! their own fields.

use serde::{Deserialize, Serialize};

use super::{SchemaError, ToolCall, ToolResult, error::excerpt};

/// The semantic role of a message.
///
/// The four values are the roles the four target protocols express. A codec maps its own wire role
/// onto one of these and rejects a role it cannot map; the canonical model never invents one.
///
/// There is deliberately no `tool` role: a tool's answer is [`Item::ToolResult`], which carries the
/// id of the call it answers. A role value that every constructor rejects would give each codec two
/// spellings of one fact and no way to choose between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Role {
    /// Instructions that govern the whole conversation.
    System,
    /// Instructions from the application developer; newer protocols carry these separately.
    Developer,
    /// Input from the caller.
    User,
    /// Output produced by the model.
    Assistant,
}

/// One input or output of a conversation.
///
/// Tool calls and results are separate items rather than message content, and reasoning is an item
/// of its own so a provider's `reasoning_content` or signed thinking block survives a round trip
/// without being folded into visible text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum Item {
    /// A message the model reads or produced.
    Message(Message),
    /// A call the model asked the caller to make.
    ToolCall(ToolCall),
    /// What the caller produced for a call.
    ToolResult(ToolResult),
    /// Hidden reasoning the model produced, possibly with a provider signature.
    Reasoning(Reasoning),
}

impl Item {
    /// Checks the item and everything it holds.
    ///
    /// # Errors
    /// Returns the validation error of the item's payload.
    pub fn validate(&self) -> Result<(), SchemaError> {
        match self {
            Self::Message(message) => message.validate(),
            Self::ToolCall(call) => call.validate(),
            Self::ToolResult(result) => result.validate(),
            Self::Reasoning(reasoning) => reasoning.validate(),
        }
    }
}

/// One message in a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    role: Role,
    content: Vec<ContentPart>,
}

impl Message {
    /// Builds a message.
    ///
    /// # Errors
    /// Returns [`SchemaError::EmptyContent`] when `content` is empty, or the validation error of a
    /// content part.
    pub fn try_new(role: Role, content: Vec<ContentPart>) -> Result<Self, SchemaError> {
        let message = Self { role, content };
        message.validate()?;
        Ok(message)
    }

    /// Builds a user message with a single text part.
    ///
    /// The constructor is infallible because one text part always satisfies the message invariant;
    /// a caller that needs a different role or several parts uses [`Message::try_new`].
    #[must_use]
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentPart::text(text)],
        }
    }

    /// Checks the content is non-empty and valid.
    ///
    /// # Errors
    /// Returns [`SchemaError::EmptyContent`] or the validation error of a content part.
    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.content.is_empty() {
            return Err(SchemaError::EmptyContent);
        }
        for part in &self.content {
            part.validate()?;
        }
        Ok(())
    }

    /// Returns the message role.
    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    /// Returns the message content.
    #[must_use]
    pub fn content(&self) -> &[ContentPart] {
        &self.content
    }
}

/// One part of a message's content.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ContentPart {
    /// Visible text.
    Text(String),
    /// An image referenced by URL or carried inline.
    Image(ImageSource),
    /// A file carried inline.
    File {
        /// The file's media type, for example `application/pdf`.
        media_type: String,
        /// The file bytes.
        data: Vec<u8>,
        /// The file name, when the protocol carries one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

impl ContentPart {
    /// Builds a text part.
    #[must_use]
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }

    /// Checks the part's payload.
    ///
    /// # Errors
    /// Returns the validation error of an image source, or [`SchemaError::Empty`] for a file with
    /// no media type or an empty name.
    pub fn validate(&self) -> Result<(), SchemaError> {
        match self {
            Self::Text(_) => Ok(()),
            Self::Image(source) => source.validate(),
            Self::File {
                media_type,
                name,
                data: _,
            } => {
                validate_media_type("content_part.media_type", media_type)?;
                if let Some(name) = name {
                    if name.trim().is_empty() {
                        return Err(SchemaError::Empty {
                            field: "content_part.name",
                        });
                    }
                }
                Ok(())
            }
        }
    }
}

/// Where an image's bytes live.
///
/// Exactly one of the two spellings holds: a URL the provider fetches, or inline base64 data with
/// its media type. The enum is the exclusivity — there is no state in which both or neither is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ImageSource {
    /// A remote `http`/`https` URL.
    Url(String),
    /// Inline image bytes with their media type.
    Base64 {
        /// The image's media type, for example `image/png`.
        media_type: String,
        /// The image bytes.
        data: Vec<u8>,
    },
}

impl ImageSource {
    /// Checks the source carries the payload its variant promises.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] for an empty URL, media type or data buffer,
    /// [`SchemaError::Invalid`] for a non-HTTP URL or a media type without `/`.
    pub fn validate(&self) -> Result<(), SchemaError> {
        match self {
            Self::Url(url) => {
                if url.trim().is_empty() {
                    return Err(SchemaError::Empty { field: "image.url" });
                }
                if !has_http_scheme(url) {
                    return Err(SchemaError::Invalid {
                        field: "image.url",
                        value: excerpt(url),
                    });
                }
                Ok(())
            }
            Self::Base64 { media_type, data } => {
                validate_media_type("image.media_type", media_type)?;
                if data.is_empty() {
                    return Err(SchemaError::Empty {
                        field: "image.data",
                    });
                }
                Ok(())
            }
        }
    }

    /// Returns the URL when the image is remote.
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Url(url) => Some(url),
            Self::Base64 { .. } => None,
        }
    }

    /// Returns the media type and bytes when the image is inline.
    #[must_use]
    pub fn base64(&self) -> Option<(&str, &[u8])> {
        match self {
            Self::Url(_) => None,
            Self::Base64 { media_type, data } => Some((media_type, data)),
        }
    }
}

/// Reasoning the model produced but did not show as visible text.
///
/// Some providers sign it so it can be sent back on the next turn; the signature is carried
/// verbatim and is opaque to the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reasoning {
    text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature: Option<String>,
}

impl Reasoning {
    /// Builds a reasoning item.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] when the text is empty or only whitespace, or when a
    /// signature is present but empty.
    pub fn try_new(
        text: impl Into<String>,
        signature: Option<String>,
    ) -> Result<Self, SchemaError> {
        let reasoning = Self {
            text: text.into(),
            signature,
        };
        reasoning.validate()?;
        Ok(reasoning)
    }

    /// Checks the text and optional signature are non-empty.
    ///
    /// # Errors
    /// Returns [`SchemaError::Empty`] for either field.
    pub fn validate(&self) -> Result<(), SchemaError> {
        if self.text.trim().is_empty() {
            return Err(SchemaError::Empty {
                field: "reasoning.text",
            });
        }
        if let Some(signature) = &self.signature {
            if signature.is_empty() {
                return Err(SchemaError::Empty {
                    field: "reasoning.signature",
                });
            }
        }
        Ok(())
    }

    /// Returns the reasoning text.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Returns the provider signature, when there is one.
    #[must_use]
    pub fn signature(&self) -> Option<&str> {
        self.signature.as_deref()
    }
}

fn validate_media_type(field: &'static str, media_type: &str) -> Result<(), SchemaError> {
    if media_type.trim().is_empty() {
        return Err(SchemaError::Empty { field });
    }
    if !media_type.contains('/') {
        return Err(SchemaError::Invalid {
            field,
            value: excerpt(media_type),
        });
    }
    Ok(())
}

fn has_http_scheme(url: &str) -> bool {
    url.get(..8)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
        || url
            .get(..7)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("http://"))
}
