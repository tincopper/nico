//! The WASM fixtures the E2E scenarios load: two hand-written modules, compiled to binary at test
//! time by the `wat` crate — the CI toolchain stays a Rust one.

// The panic-free policy protects request paths. A fixture that cannot compile is a broken test, not
// a runtime condition, and clippy only recognises the lint exemption inside `#[test]` functions, not
// in the helpers they call.
/// A module that refuses every request (the `refuse` action).
pub(crate) const REFUSE_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (data (i32.const 2048) "{\"action\":\"refuse\"}")
  (func (export "nico_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_request") (param i32 i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (i32.const 2048)) (i64.const 32))
      (i64.extend_i32_u (i32.const 19)))))
"#;

/// A module that continues on every stage.
pub(crate) const CONTINUE_WAT: &str = r#"
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

/// A module that loops forever in `on_request`: fuel is what stops it.
pub(crate) const FUEL_HOG_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "nico_alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "on_request") (param i32 i32) (result i64)
    (loop br 0)
    (i64.const 0)))
"#;

/// Compiles one fixture's text into the bytes a `kind: wasm` module path names.
///
/// # Panics
/// When `wat_text` is not a valid module: a broken fixture is a broken test, and panicking at the
/// fixture site names it.
#[must_use]
pub(crate) fn compiled(wat_text: &str) -> Vec<u8> {
    wat::parse_str(wat_text).expect("the fixture wat parses")
}
