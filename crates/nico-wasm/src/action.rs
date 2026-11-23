//! The action a module answers a stage with, decoded from its output bytes.

use serde::Deserialize;
use thiserror::Error;

/// The action one plugin stage may take.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Let the stage continue unchanged.
    Continue,
    /// Refuse the caller with the publication's compiled presentation: the module decided
    /// *whether*, the document decides *how the refusal looks*.
    Refuse,
    /// Answer the caller here: the status, headers and body the response is written with.
    Respond {
        /// The response status.
        status: u16,
        /// The response headers, as the module wrote them.
        headers: Vec<(String, String)>,
        /// The response body.
        body: String,
    },
}

/// Why a module's output could not be read as an [`Action`].
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ActionError {
    /// The output was not valid JSON, or was not one of the known actions.
    #[error("{0}")]
    Invalid(String),
    /// The module answered a refusal without naming a status.
    #[error("the respond action carries no status")]
    NoStatus,
    /// The module answered a refusal with a status outside the caller-fault range.
    #[error("the respond status {0} is outside the 4xx range a plugin refusal may use")]
    StatusRange(u16),
}

/// Decodes one stage's output bytes into an [`Action`].
///
/// # Errors
/// Returns [`ActionError`] when the bytes are not a readable action, the respond action carries no
/// status, or its status is outside the caller-fault range a plugin refusal may use.
pub fn decode(output: &[u8]) -> Result<Action, ActionError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Wire {
        action: String,
        status: Option<u16>,
        #[serde(default)]
        headers: Vec<(String, String)>,
        #[serde(default)]
        body: String,
    }

    let wire: Wire =
        serde_json::from_slice(output).map_err(|error| ActionError::Invalid(error.to_string()))?;
    match wire.action.as_str() {
        "continue" => Ok(Action::Continue),
        // A refusal takes the publication's presentation: the module decides *whether*, the document
        // decides *how the refusal looks*. The host's caller maps [`Action::Refuse`] onto the
        // compiled rejection.
        "refuse" => Ok(Action::Refuse),
        "respond" => {
            let status = wire.status.ok_or(ActionError::NoStatus)?;
            if !(400..500).contains(&status) {
                return Err(ActionError::StatusRange(status));
            }
            Ok(Action::Respond {
                status,
                headers: wire.headers,
                body: wire.body,
            })
        }
        other => Err(ActionError::Invalid(format!("unknown action `{other}`"))),
    }
}
