//! Decoding the document around the conversation: system, sampling, tools and preserved fields.

use nico_llm::{CodecError, Item, Role};

use crate::support::{self, refusal};

/// A minimal document: one user turn of text.
const BODY: &str = r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#;

/// The smallest document decodes to one user message with the path's model.
#[test]
fn a_minimal_document_decodes_to_one_user_message() {
    let request = support::decode(BODY).expect("a minimal document decodes");
    assert_eq!(request.model().as_str(), support::MODEL);
    assert!(!request.stream());
    assert_eq!(request.conversation().len(), 1);
    let Item::Message(message) = &request.conversation()[0] else {
        panic!("one message, got {:?}", request.conversation()[0]);
    };
    assert_eq!(message.role(), Role::User);
}

/// The system instruction is the leading system item, text only.
#[test]
fn the_system_instruction_becomes_the_leading_system_item() {
    let request = support::decode(
        r#"{"systemInstruction":{"parts":[{"text":"be brief"},{"text":"be kind"}]},"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
    )
    .expect("a document with an instruction decodes");
    let Item::Message(system) = &request.conversation()[0] else {
        panic!("a system item first");
    };
    assert_eq!(system.role(), Role::System);
    assert_eq!(system.content().len(), 2, "both parts are kept in order");

    let error = refusal(
        r#"{"systemInstruction":{"parts":[{"inlineData":{"mimeType":"image/png","data":"aGVsbG8="}}]},"contents":[{"role":"user","parts":[{"text":"hello"}]}]}"#,
    );
    assert!(
        matches!(error, CodecError::UnsupportedField { ref field, .. } if field == "systemInstruction.parts"),
        "the instruction is text-only, got {error:?}"
    );
}

/// A turn without a role is the caller's, per the protocol's own single-turn default.
#[test]
fn a_turn_without_a_role_is_the_callers() {
    let request = support::decode(r#"{"contents":[{"parts":[{"text":"hello"}]}]}"#)
        .expect("the role is optional for single-turn requests");
    let Item::Message(message) = &request.conversation()[0] else {
        panic!("one message");
    };
    assert_eq!(message.role(), Role::User);

    let error = refusal(r#"{"contents":[{"role":"system","parts":[{"text":"hello"}]}]}"#);
    assert!(
        matches!(error, CodecError::InvalidRequest { .. }),
        "the system prompt is `systemInstruction`, not a turn, got {error:?}"
    );
}

/// Empty `contents` is not a conversation.
#[test]
fn empty_contents_is_not_a_conversation() {
    for body in [
        r#"{}"#,
        r#"{"contents":[]}"#,
        r#"{"systemInstruction":{"parts":[{"text":"be brief"}]}}"#,
    ] {
        let error = refusal(body);
        assert!(
            matches!(error, CodecError::InvalidRequest { .. }),
            "`{body}` carries no turn, got {error:?}"
        );
    }
}

/// Every canonical sampling knob has a member in `generationConfig`, so nothing is refused.
#[test]
fn generation_config_carries_every_canonical_knob() {
    let request = support::decode(
        r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{
            "temperature":1.5,"topP":0.9,"maxOutputTokens":64,"stopSequences":["END","STOP"],
            "presencePenalty":0.5,"frequencyPenalty":-0.5,"seed":7
        }}"#,
    )
    .expect("a full generationConfig decodes");
    let sampling = request.sampling();
    assert_eq!(sampling.temperature(), Some(1.5));
    assert_eq!(sampling.top_p(), Some(0.9));
    assert_eq!(sampling.max_tokens(), Some(64));
    assert_eq!(sampling.stop(), ["END", "STOP"]);
    assert_eq!(sampling.presence_penalty(), Some(0.5));
    assert_eq!(sampling.frequency_penalty(), Some(-0.5));
    assert_eq!(sampling.seed(), Some(7));
}

/// The knobs `generationConfig` defines and the canonical model cannot carry are refused by name.
#[test]
fn the_unrepresentable_generation_knobs_are_refused_by_name() {
    for (member, expected) in [
        (r#""topK": 4"#, "generationConfig.topK"),
        (
            r#""thinkingConfig": {"thinkingBudget": 100}"#,
            "generationConfig.thinkingConfig",
        ),
        (r#""candidateCount": 3"#, "generationConfig.candidateCount"),
    ] {
        let body = format!(
            r#"{{"contents":[{{"role":"user","parts":[{{"text":"hello"}}]}}],"generationConfig":{{{member}}}}}"#
        );
        let error = refusal(&body);
        assert!(
            matches!(error, CodecError::UnsupportedField { ref field, .. } if field.as_ref() == expected),
            "`{member}` must name `{expected}`, got {error:?}"
        );
    }
}

/// One candidate is what the canonical model produces anyway, so `candidateCount: 1` is accepted.
#[test]
fn a_single_candidate_count_is_accepted() {
    support::decode(
        r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{"candidateCount":1}}"#,
    )
    .expect("one candidate is the default, not a request the gateway cannot serve");
}

/// The structured-output spellings map; anything else is refused.
#[test]
fn the_response_mime_type_maps_the_structured_formats() {
    let object = support::decode(
        r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{"responseMimeType":"application/json"}}"#,
    )
    .expect("a json mime decodes");
    assert!(matches!(
        object.response_format(),
        nico_llm::ResponseFormat::JsonObject
    ));

    let schema = support::decode(
        r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{"responseMimeType":"application/json","responseSchema":{"type":"object"}}}"#,
    )
    .expect("a json mime with a schema decodes");
    let nico_llm::ResponseFormat::JsonSchema(schema) = schema.response_format() else {
        panic!("a schema format");
    };
    assert!(
        schema.name().is_none(),
        "this dialect identifies a schema by position"
    );
    assert_eq!(schema.schema().to_text(), r#"{"type":"object"}"#);

    support::decode(
        r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{"responseMimeType":"text/plain"}}"#,
    )
    .expect("the default spelling is free-form text");

    let error = refusal(
        r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{"responseMimeType":"text/csv"}}"#,
    );
    assert!(
        matches!(error, CodecError::UnsupportedField { ref field, .. } if field == "generationConfig.responseMimeType"),
        "got {error:?}"
    );

    let error = refusal(
        r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"generationConfig":{"responseSchema":{"type":"object"}}}"#,
    );
    assert!(
        matches!(error, CodecError::InvalidRequest { .. }),
        "a schema without the json mime is an inconsistent document, got {error:?}"
    );
}

/// Declared functions decode; the built-in tools are refused where they are written.
#[test]
fn function_declarations_decode_and_builtin_tools_are_refused() {
    let request = support::decode(
        r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"tools":[
            {"functionDeclarations":[{"name":"lookup","description":"finds","parameters":{"type":"object"}}]}
        ]}"#,
    )
    .expect("a declaration decodes");
    assert_eq!(request.tools().len(), 1);
    assert_eq!(request.tools()[0].name().as_str(), "lookup");

    for builtin in ["googleSearch", "codeExecution", "urlContext"] {
        let body = format!(
            r#"{{"contents":[{{"role":"user","parts":[{{"text":"hello"}}]}}],"tools":[{{"{builtin}": {{}}}}]}}"#
        );
        let error = refusal(&body);
        assert!(
            matches!(error, CodecError::UnsupportedField { ref field, .. }
                if field.as_ref() == format!("tools[0].{builtin}")),
            "`{builtin}` runs at the provider, got {error:?}"
        );
    }
}

/// The tool-choice modes map; `allowedFunctionNames` beside any mode but `ANY` is a misshape.
#[test]
fn the_tool_config_modes_map() {
    let body = |config: &str| {
        format!(
            r#"{{"contents":[{{"role":"user","parts":[{{"text":"hello"}}]}}],"tools":[{{"functionDeclarations":[{{"name":"lookup","parameters":{{"type":"object"}}}}]}}],"toolConfig":{config}}}"#
        )
    };
    let decoded = |config: &str| support::decode(&body(config)).expect("a tool config decodes");
    assert!(matches!(
        decoded(r#"{"functionCallingConfig":{"mode":"AUTO"}}"#).tool_choice(),
        nico_llm::ToolChoice::Auto
    ));
    assert!(matches!(
        decoded(r#"{"functionCallingConfig":{"mode":"NONE"}}"#).tool_choice(),
        nico_llm::ToolChoice::None
    ));
    assert!(matches!(
        decoded(r#"{"functionCallingConfig":{"mode":"ANY"}}"#).tool_choice(),
        nico_llm::ToolChoice::Required
    ));
    assert!(matches!(
        decoded(r#"{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["lookup"]}}"#)
            .tool_choice(),
        nico_llm::ToolChoice::Tool(_)
    ));

    for config in [
        r#"{"functionCallingConfig":{"mode":"MAGIC"}}"#,
        r#"{"functionCallingConfig":{"allowedFunctionNames":["lookup"]}}"#,
        r#"{"functionCallingConfig":{"mode":"AUTO","allowedFunctionNames":["lookup"]}}"#,
        r#"{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["lookup","other"]}}"#,
    ] {
        let error = refusal(&body(config));
        assert!(
            matches!(error, CodecError::InvalidRequest { .. })
                || matches!(error, CodecError::UnsupportedField { .. }),
            "`{config}` is refused, got {error:?}"
        );
    }
}

/// `labels` is preserved; the side-effect members are refused by name; the rest is preserved too.
#[test]
fn top_level_fields_are_preserved_or_refused() {
    let request = support::decode(
        r#"{"contents":[{"role":"user","parts":[{"text":"hello"}]}],"labels":{"team":"gateway"},"tags":{"x":1}}"#,
    )
    .expect("preserved fields decode");
    assert_eq!(
        request.extensions().get("labels").map(|v| v.to_text()),
        Some(r#"{"team":"gateway"}"#.to_owned())
    );
    assert!(request.extensions().get("tags").is_some());

    for (member, expected) in [
        (
            r#""safetySettings": [{"category":"HARM_CATEGORY_1"}]"#,
            "safetySettings",
        ),
        (r#""cachedContent": "cachedContents/abc""#, "cachedContent"),
        (r#""model": "gemini-2.0-flash""#, "model"),
    ] {
        let body =
            format!(r#"{{"contents":[{{"role":"user","parts":[{{"text":"hello"}}]}}],{member}}}"#);
        let error = refusal(&body);
        let is_model = expected == "model";
        assert!(
            if is_model {
                matches!(error, CodecError::InvalidRequest { .. })
            } else {
                matches!(error, CodecError::UnsupportedField { ref field, .. } if field.as_ref() == expected)
            },
            "`{member}` must be refused naming `{expected}`, got {error:?}"
        );
    }
}
