//! The sandbox, against hand-written `.wat` fixtures: what a module's action becomes, and what the
//! budget does to a module that overruns it.
//!
//! The fixtures are text compiled by the `wat` crate at test time, so the CI toolchain stays a Rust
//! one — no Rust-to-WASM cross-compilation is exercised anywhere in this tree.

use super::*;

/// A module that answers `continue` for every export.
const CONTINUE_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "nico_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_request") (param i32 i32) (result i64)
    (i64.const 0))
  (func (export "on_response") (param i32 i32) (result i64)
    (i64.const 0))
  (func (export "on_logging") (param i32 i32) (result i64)
    (i64.const 0)))
"#;

/// A module whose `on_request` writes a `refuse` action into memory and returns its coordinates.
const REFUSE_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 2048) "{\"action\":\"refuse\"}")
  (func (export "nico_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_request") (param i32 i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (i32.const 2048)) (i64.const 32))
      (i64.extend_i32_u (i32.const 19)))))
"#;

/// A module that loops forever: fuel is what stops it.
const FUEL_HOG_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "nico_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_request") (param i32 i32) (result i64)
    (loop br 0)
    (i64.const 0)))
"#;

/// A module that writes output but returns a length the memory cannot back.
const OUT_OF_RANGE_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "nico_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_request") (param i32 i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (i32.const 1024)) (i64.const 32))
      (i64.extend_i32_u (i32.const 0xFFFFF)))))
"#;

/// A module that provides only `nico_alloc` and `memory`: every stage export is missing.
const NO_EXPORTS_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "nico_alloc") (param i32) (result i32) (i32.const 1024)))
"#;

/// Compiles a `.wat` fixture with the host's own engine configuration.
fn compile(wat_text: &str, resources: WasmResources) -> WasmPlugin {
    let module = wat::parse_str(wat_text).expect("the fixture wat parses");
    let bytes = wasm_bytes(module);
    WasmPlugin::compile(&bytes, resources).expect("the fixture compiles")
}

/// Serializes a parsed module into the bytes [`WasmPlugin::compile`] consumes.
fn wasm_bytes(module: Vec<u8>) -> Vec<u8> {
    module
}

fn facts<'a>() -> RequestFacts<'a> {
    RequestFacts {
        method: "GET",
        path: "/api/items",
        headers: &[],
        subject: None,
        tenant: None,
    }
}

#[test]
fn request_facts_can_carry_a_tenant_without_changing_absence() {
    let tenant_facts = RequestFacts {
        method: "GET",
        path: "/api/items",
        headers: &[],
        subject: Some("key-a"),
        tenant: Some("tenant-a"),
    };
    let encoded = serde_json::to_value(&tenant_facts).expect("facts serialize");
    assert_eq!(encoded["subject"], "key-a");
    assert_eq!(encoded["tenant"], "tenant-a");
    assert_eq!(
        serde_json::to_value(facts()).expect("facts serialize")["tenant"],
        serde_json::Value::Null
    );
}

#[test]
fn a_continue_action_passes_the_stage_through() {
    let plugin = compile(CONTINUE_WAT, WasmResources::default());
    let action = plugin
        .on_request(&facts(), &serde_json::json!(null))
        .expect("continues");
    assert_eq!(action, Action::Continue);
}

#[test]
fn a_refuse_action_takes_the_publication_s_status() {
    let plugin = compile(REFUSE_WAT, WasmResources::default());
    let action = plugin
        .on_request(&facts(), &serde_json::json!(null))
        .expect("refuses");
    assert_eq!(
        action,
        Action::Refuse,
        "the module decides whether; the host's caller maps it onto the compiled rejection"
    );
}

#[test]
fn a_module_that_loops_forever_exhausts_its_fuel() {
    let plugin = compile(
        FUEL_HOG_WAT,
        WasmResources {
            fuel: 100_000,
            memory_pages: 16,
        },
    );
    let error = plugin
        .on_request(&facts(), &serde_json::json!(null))
        .expect_err("the loop cannot end within the budget");
    assert!(
        matches!(error, WasmPluginError::Budget { .. }),
        "fuel exhaustion is a budget failure, got {error:?}"
    );
}

#[test]
fn a_module_reporting_output_beyond_its_memory_is_a_budget_failure() {
    let plugin = compile(OUT_OF_RANGE_WAT, WasmResources::default());
    let error = plugin
        .on_request(&facts(), &serde_json::json!(null))
        .expect_err("the declared output runs past the memory's end");
    assert!(matches!(error, WasmPluginError::Budget { .. }));
}

#[test]
fn a_module_without_a_stage_export_names_it() {
    // The host's own rule is "a missing export means Continue"; the caller implements that. What
    // this test pins is the host's half: asking for `on_request` reports the missing export rather
    // than trapping.
    let plugin = compile(NO_EXPORTS_WAT, WasmResources::default());
    let error = plugin
        .on_request(&facts(), &serde_json::json!(null))
        .expect_err("there is no on_request export");
    assert!(
        matches!(
            error,
            WasmPluginError::Export {
                export: "on_request"
            }
        ),
        "the refusal names the missing export, got {error:?}"
    );
}

#[test]
fn an_invalid_artifact_is_a_compile_failure() {
    let error = WasmPlugin::compile(b"not a module", WasmResources::default())
        .expect_err("the bytes are not a WebAssembly module");
    assert!(matches!(error, WasmPluginError::Compile { .. }));
}

#[test]
fn an_action_outside_the_caller_fault_range_is_refused_by_the_decoder() {
    let error = action::decode(br#"{"action":"respond","status":503}"#)
        .expect_err("a plugin refusal is a caller fault: 5xx would misdirect monitoring");
    assert!(
        matches!(error, ActionError::StatusRange(503)),
        "the decoder rejects a 5xx respond, got {error:?}"
    );
}

#[test]
fn a_respond_action_without_a_status_is_refused_by_the_decoder() {
    let error = action::decode(br#"{"action":"respond"}"#)
        .expect_err("the document's refusal status is the fallback, not a guess");
    assert!(matches!(error, ActionError::NoStatus));
}
