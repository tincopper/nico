//! Every validation path the canonical *request and response trees* promise to reject.
//!
//! The leaf values (`ModelId`, names, image/file parts, `CanonicalError`, `RawJson`, `UsageCounts`)
//! are covered in `schema_values.rs`.

#![expect(
    clippy::expect_used,
    reason = "a fixture that cannot build its value has nothing useful to return"
)]

use nico_llm::{
    AssistantTurn, ChatRequest, ChatResponse, Choice, ContentPart, Extensions, FinishReason, Item,
    JsonSchema, Message, ModelId, RawJson, Reasoning, ResponseId, Role, Sampling, SchemaError,
    Tool, ToolCall, ToolCallId, ToolChoice, ToolName, ToolResult,
};
use serde_json::json;

fn model() -> ModelId {
    ModelId::try_new("gpt-4o-mini").expect("valid model")
}

fn tool_name(name: &str) -> ToolName {
    ToolName::try_new(name).expect("valid tool name")
}

fn call_id(id: &str) -> ToolCallId {
    ToolCallId::try_new(id).expect("valid call id")
}

fn message(text: &str) -> Item {
    Item::Message(
        Message::try_new(Role::User, vec![ContentPart::text(text)]).expect("valid message"),
    )
}

fn request() -> ChatRequest {
    ChatRequest::try_new(model(), vec![message("hi")]).expect("valid request")
}

fn tool(name: &str) -> Tool {
    Tool::try_new(tool_name(name), RawJson::new(json!({ "type": "object" }))).expect("valid tool")
}

#[test]
fn an_empty_conversation_is_rejected() {
    let error = ChatRequest::try_new(model(), Vec::new()).expect_err("empty conversation");
    assert_eq!(error, SchemaError::EmptyConversation);
}

#[test]
fn the_tool_role_has_no_canonical_spelling() {
    assert!(
        serde_json::from_str::<Role>(r#""tool""#).is_err(),
        "a tool answer is an Item::ToolResult, not a message role"
    );
    let message = r#"{"role":"tool","content":[{"type":"text","data":"ok"}]}"#;
    assert!(
        serde_json::from_str::<Message>(message).is_err(),
        "a message cannot claim the tool role either"
    );
}

#[test]
fn a_message_needs_at_least_one_content_part() {
    let error = Message::try_new(Role::User, Vec::new()).expect_err("empty content");
    assert_eq!(error, SchemaError::EmptyContent);
}

#[test]
fn a_tool_result_needs_at_least_one_content_part() {
    let error = ToolResult::try_new(call_id("call_1"), Vec::new()).expect_err("empty content");
    assert_eq!(
        error,
        SchemaError::Empty {
            field: "tool_result.content"
        }
    );
}

#[test]
fn a_reasoning_item_needs_text() {
    let error = Reasoning::try_new("  ", None).expect_err("empty reasoning");
    assert_eq!(
        error,
        SchemaError::Empty {
            field: "reasoning.text"
        }
    );
}

#[test]
fn duplicate_tool_names_are_rejected() {
    let error = request()
        .with_tools(vec![tool("describe"), tool("describe")])
        .expect_err("duplicate tool names");
    assert_eq!(
        error,
        SchemaError::Duplicate {
            kind: "tool",
            value: "describe".to_owned()
        }
    );
}

#[test]
fn a_named_tool_choice_must_be_declared() {
    let error = request()
        .with_tools(vec![tool("describe")])
        .expect("declared tool")
        .with_tool_choice(ToolChoice::Tool(tool_name("summarise")))
        .expect_err("undeclared choice");
    assert_eq!(
        error,
        SchemaError::UndeclaredTool {
            name: "summarise".to_owned()
        }
    );
}

#[test]
fn tool_arguments_must_be_a_json_object() {
    let error = ToolCall::try_new(
        call_id("call_1"),
        tool_name("describe"),
        RawJson::new(json!([1, 2])),
    )
    .expect_err("array arguments");
    assert_eq!(
        error,
        SchemaError::NotAnObject {
            field: "tool_call.arguments"
        }
    );
}

#[test]
fn tool_parameters_must_be_a_json_object() {
    let error = Tool::try_new(tool_name("describe"), RawJson::new(json!("not-a-schema")))
        .expect_err("scalar parameters");
    assert_eq!(
        error,
        SchemaError::NotAnObject {
            field: "tool.parameters"
        }
    );
}

#[test]
fn a_json_schema_must_declare_at_least_one_field() {
    assert_eq!(
        JsonSchema::try_new(None, RawJson::new(json!({}))).expect_err("empty schema"),
        SchemaError::EmptyObject {
            field: "response_format.json_schema"
        }
    );
    assert!(matches!(
        JsonSchema::try_new(None, RawJson::new(json!("text"))),
        Err(SchemaError::NotAnObject {
            field: "response_format.json_schema"
        })
    ));
    assert_eq!(
        JsonSchema::try_new(
            Some("  ".to_owned()),
            RawJson::new(json!({ "type": "object" }))
        )
        .expect_err("empty schema name"),
        SchemaError::Empty {
            field: "response_format.json_schema.name"
        }
    );
}

#[test]
fn sampling_rejects_non_finite_and_out_of_range_values() {
    assert_eq!(
        Sampling::default()
            .with_temperature(f64::NAN)
            .expect_err("NaN"),
        SchemaError::NotFinite {
            field: "sampling.temperature"
        }
    );
    assert!(matches!(
        Sampling::default().with_temperature(f64::INFINITY),
        Err(SchemaError::NotFinite {
            field: "sampling.temperature"
        })
    ));
    assert!(matches!(
        Sampling::default().with_temperature(2.1),
        Err(SchemaError::OutOfRange {
            field: "sampling.temperature",
            ..
        })
    ));
    assert!(matches!(
        Sampling::default().with_top_p(1.1),
        Err(SchemaError::OutOfRange {
            field: "sampling.top_p",
            ..
        })
    ));
    assert!(matches!(
        Sampling::default().with_max_tokens(0),
        Err(SchemaError::OutOfRange {
            field: "sampling.max_tokens",
            ..
        })
    ));
    assert!(
        Sampling::try_new(Some(0.0), Some(0.0), Some(1)).is_ok(),
        "the boundaries are inclusive"
    );
}

#[test]
fn sampling_models_the_knobs_every_target_protocol_carries() {
    let sampling = Sampling::default()
        .with_temperature(0.2)
        .expect("temperature in range")
        .with_stop(vec!["\n\n".to_owned()])
        .expect("one stop sequence")
        .with_seed(7)
        .with_presence_penalty(0.1)
        .expect("presence penalty in range")
        .with_frequency_penalty(-0.2)
        .expect("frequency penalty in range");
    assert_eq!(sampling.temperature(), Some(0.2));
    assert_eq!(sampling.stop(), ["\n\n".to_owned()]);
    assert_eq!(sampling.seed(), Some(7));
    assert_eq!(sampling.presence_penalty(), Some(0.1));
    assert_eq!(sampling.frequency_penalty(), Some(-0.2));
    assert_eq!(
        Sampling::default()
            .with_presence_penalty(-2.0)
            .expect("the lower bound is inclusive")
            .presence_penalty(),
        Some(-2.0)
    );
}

#[test]
fn sampling_rejects_stop_sequences_and_penalties_it_cannot_bound() {
    let five = vec!["a".to_owned(); 5];
    assert!(matches!(
        Sampling::default().with_stop(five),
        Err(SchemaError::OutOfRange {
            field: "sampling.stop",
            ..
        })
    ));
    assert_eq!(
        Sampling::default()
            .with_stop(vec![String::new()])
            .expect_err("an empty stop sequence stops nothing"),
        SchemaError::Empty {
            field: "sampling.stop"
        }
    );
    assert!(matches!(
        Sampling::default().with_presence_penalty(2.1),
        Err(SchemaError::OutOfRange {
            field: "sampling.presence_penalty",
            ..
        })
    ));
    assert!(matches!(
        Sampling::default().with_frequency_penalty(f64::NAN),
        Err(SchemaError::NotFinite {
            field: "sampling.frequency_penalty"
        })
    ));
}

#[test]
fn sampling_rejects_a_field_it_does_not_model() {
    let error =
        serde_json::from_str::<Sampling>(r#"{"min_p":0.1}"#).expect_err("unknown sampling field");
    assert!(
        error.to_string().contains("min_p"),
        "the rejection must name the field: {error}"
    );
}

#[test]
fn a_response_needs_at_least_one_choice() {
    let error = ChatResponse::try_new(
        ResponseId::try_new("resp_1").expect("id"),
        model(),
        Vec::new(),
    )
    .expect_err("empty choices");
    assert_eq!(error, SchemaError::EmptyChoices);
}

#[test]
fn a_response_cannot_repeat_a_choice_index() {
    let turn = AssistantTurn::try_new(vec![ContentPart::text("hi")], Vec::new()).expect("turn");
    let choice = Choice::try_new(0, turn, FinishReason::Stop).expect("choice");
    let error = ChatResponse::try_new(
        ResponseId::try_new("resp_1").expect("id"),
        model(),
        vec![choice.clone(), choice],
    )
    .expect_err("duplicate index");
    assert_eq!(error, SchemaError::DuplicateChoiceIndex { index: 0 });
}

#[test]
fn an_assistant_turn_cannot_repeat_a_call_id() {
    let call = ToolCall::try_new(
        call_id("call_1"),
        tool_name("describe"),
        RawJson::new(json!({})),
    )
    .expect("valid call");
    let error = AssistantTurn::try_new(Vec::new(), vec![call.clone(), call])
        .expect_err("duplicate call id");
    assert_eq!(
        error,
        SchemaError::Duplicate {
            kind: "tool call id",
            value: "call_1".to_owned()
        }
    );
}

#[test]
fn an_extension_key_must_not_be_empty_or_shadow_a_modelled_field() {
    let mut extensions = Extensions::new();
    assert_eq!(
        extensions.try_insert("", json!(1)).expect_err("empty key"),
        SchemaError::Empty {
            field: "extensions key"
        }
    );
    extensions
        .try_insert("model", json!("other-model"))
        .expect("non-empty key");
    let error = request()
        .with_extensions(extensions)
        .validate()
        .expect_err("shadowing extension key");
    assert_eq!(
        error,
        SchemaError::ReservedExtensionKey {
            key: "model".to_owned()
        }
    );
}

#[test]
fn an_empty_extension_key_from_the_wire_is_rejected_by_validate() {
    let raw = r#"{
        "model": "gpt-4o-mini",
        "conversation": [
            {
                "type": "message",
                "data": {
                    "role": "user",
                    "content": [{ "type": "text", "data": "hi" }]
                }
            }
        ],
        "": 1
    }"#;
    let request: ChatRequest = serde_json::from_str(raw).expect("the shape decodes");
    assert_eq!(
        request.validate(),
        Err(SchemaError::Empty {
            field: "extensions key"
        }),
        "an empty key cannot be written back into a JSON object"
    );
}

#[test]
fn a_response_extension_key_must_not_shadow_a_modelled_field() {
    let turn = AssistantTurn::try_new(vec![ContentPart::text("hi")], Vec::new()).expect("turn");
    let choice = Choice::try_new(0, turn, FinishReason::Stop).expect("choice");
    let response = ChatResponse::try_new(
        ResponseId::try_new("resp_1").expect("id"),
        model(),
        vec![choice],
    )
    .expect("valid response");
    let mut extensions = Extensions::new();
    extensions
        .try_insert("choices", json!([]))
        .expect("non-empty key");
    assert_eq!(
        response.with_extensions(extensions).validate(),
        Err(SchemaError::ReservedExtensionKey {
            key: "choices".to_owned()
        })
    );
}

#[test]
fn a_deserialized_item_still_needs_validation() {
    let raw = r#"{
        "model": "gpt-4o-mini",
        "conversation": [
            { "type": "reasoning", "data": { "text": "  " } }
        ]
    }"#;
    let request: ChatRequest = serde_json::from_str(raw).expect("the shape decodes");
    assert_eq!(
        request.validate(),
        Err(SchemaError::Empty {
            field: "reasoning.text"
        }),
        "serde is not a trust boundary; validate walks the tree"
    );
}

#[test]
fn a_deserialized_request_still_needs_validation() {
    let raw = r#"{"model":"gpt-4o-mini","conversation":[]}"#;
    let request: ChatRequest = serde_json::from_str(raw).expect("the shape decodes");
    assert_eq!(
        request.validate(),
        Err(SchemaError::EmptyConversation),
        "serde is not a trust boundary; validate is"
    );
}
