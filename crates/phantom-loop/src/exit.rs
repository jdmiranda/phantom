//! Structured exit schema compiled from a raw JSON value.
//!
//! Every [`crate::LoopAgentSpec`] declares an `exit_schema` (raw `serde_json::Value`)
//! describing what a successful loop-iteration result must look like. The
//! reviewer-loop example (issue #650) carries something like:
//!
//! ```toml
//! [agent.exit_schema]
//! type = "object"
//! required = ["pr_number", "decision"]
//!
//! [agent.exit_schema.properties.pr_number]
//! type = "integer"
//!
//! [agent.exit_schema.properties.decision]
//! enum = ["approved", "rejected", "needs_changes"]
//! ```
//!
//! At spec-load time we compile that raw value into a reusable
//! [`jsonschema::Validator`] wrapped in an [`std::sync::Arc`] so the eventual
//! `LoopRunner` (C2) can share one schema across all iterations of a loop
//! without re-compiling it on every result.

use std::sync::Arc;

use jsonschema::Validator;
use serde_json::Value;

use crate::error::LoopSpecError;

/// Pre-compiled JSON Schema gating the typed "exit" payload that agents must
/// emit at the end of each loop iteration.
///
/// `ExitSchema` is cheap to clone — the inner [`Validator`] sits behind an
/// [`Arc`] so the future runner can hold one copy per spec and hand
/// references to every iteration without bumping a reference count for
/// validation alone.
#[derive(Clone)]
pub struct ExitSchema(pub Arc<Validator>);

impl std::fmt::Debug for ExitSchema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Validator` does not implement `Debug`, so we elide its contents
        // rather than leak its `Display` impl into our trait surface.
        f.debug_struct("ExitSchema")
            .field("compiled", &"<jsonschema::Validator>")
            .finish()
    }
}

impl ExitSchema {
    /// Compile a raw JSON Schema value into a reusable [`Validator`].
    ///
    /// # Errors
    ///
    /// Returns [`LoopSpecError::SchemaCompile`] if the value is not a valid
    /// JSON Schema — for example, a non-object root, an unknown keyword in a
    /// strict draft, or a `$ref` that fails to resolve.
    pub fn compile(raw: &Value) -> Result<Self, LoopSpecError> {
        let validator = jsonschema::validator_for(raw)
            .map_err(|e| LoopSpecError::SchemaCompile(e.to_string()))?;
        Ok(Self(Arc::new(validator)))
    }

    /// Check whether the given instance satisfies the schema.
    ///
    /// Returns `Ok(())` if the instance is valid, otherwise `Err(Vec<String>)`
    /// where each string describes one validation failure. The `Vec` form
    /// keeps the API allocation-free for the happy path and avoids leaking
    /// the borrowing lifetimes that `jsonschema::ErrorIterator` carries.
    ///
    /// # Errors
    ///
    /// Returns `Err` containing a non-empty `Vec` of human-readable error
    /// messages when the instance fails to validate. The vector preserves
    /// the order in which the underlying validator surfaced the errors.
    pub fn validate(&self, instance: &Value) -> Result<(), Vec<String>> {
        // `Validator::validate` returns `Err(ErrorIterator<'instance>)` on
        // failure — we materialise the iterator into owned `String`s so
        // callers don't need to hold the instance reference, and so we don't
        // leak the upstream borrowing lifetimes through our public API.
        match self.0.validate(instance) {
            Ok(()) => Ok(()),
            Err(errors) => Err(errors.map(|e| e.to_string()).collect()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compile_accepts_simple_object_schema() {
        let raw = json!({
            "type": "object",
            "required": ["x"],
            "properties": {
                "x": { "type": "integer" }
            }
        });
        let schema = ExitSchema::compile(&raw).expect("compile");
        // Valid instance.
        schema
            .validate(&json!({ "x": 1 }))
            .expect("valid instance must pass");
        // Missing required field.
        assert!(schema.validate(&json!({})).is_err());
        // Wrong type.
        assert!(schema.validate(&json!({ "x": "str" })).is_err());
    }

    #[test]
    fn compile_rejects_obviously_invalid_schema() {
        // `type` must be a string or array of strings — a bare integer is a
        // structural violation that the validator catches at compile time.
        let raw = json!({ "type": 42 });
        assert!(ExitSchema::compile(&raw).is_err());
    }

    #[test]
    fn validate_returns_multiple_errors_in_aggregate() {
        let raw = json!({
            "type": "object",
            "required": ["a", "b"],
            "properties": {
                "a": { "type": "integer" },
                "b": { "type": "integer" }
            }
        });
        let schema = ExitSchema::compile(&raw).expect("compile");
        // Missing both required keys.
        let err = schema.validate(&json!({})).unwrap_err();
        assert!(!err.is_empty(), "expected at least one error");
    }
}
