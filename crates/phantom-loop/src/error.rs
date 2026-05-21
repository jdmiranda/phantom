//! Error types for loop spec loading.
//!
//! Errors are deliberately coarse — TOML parse, schema compile, IO, semantic
//! validation. Future slices may add runtime-error variants (queue-overflow,
//! task-stalled) at which point those should live in a separate `RuntimeError`.

use std::path::PathBuf;
use thiserror::Error;

/// Anything that can go wrong while reading and compiling a [`crate::LoopSpec`].
#[derive(Debug, Error)]
pub enum LoopSpecError {
    /// Failed to read the TOML file from disk.
    #[error("failed to read loop spec at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// TOML deserialization rejected the file.
    #[error("invalid TOML in loop spec at {path}: {source}")]
    TomlParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// TOML deserialization rejected a raw string (no path context — used by
    /// in-memory parsing in tests and the future runner).
    #[error("invalid TOML in loop spec: {0}")]
    TomlParseRaw(#[from] toml::de::Error),

    /// The embedded `exit_schema` value failed JSON Schema compilation.
    ///
    /// The wrapped string carries the human-readable description the
    /// `jsonschema` crate produced — we deliberately do not store the
    /// `ValidationError<'static>` directly because it transitively borrows
    /// crate-internal state we do not want to leak through our public API.
    #[error("exit_schema is not a valid JSON Schema: {0}")]
    SchemaCompile(String),

    /// A field was present but the value violated a structural constraint that
    /// serde alone cannot express (e.g. an empty `id`).
    #[error("invalid field `{field}` in loop spec: {reason}")]
    InvalidField {
        field: &'static str,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_field_message_includes_field_and_reason() {
        let err = LoopSpecError::InvalidField {
            field: "id",
            reason: "must not be empty".to_string(),
        };
        let msg = err.to_string();
        assert!(msg.contains("id"));
        assert!(msg.contains("must not be empty"));
    }

    #[test]
    fn schema_compile_message_includes_underlying_reason() {
        let err = LoopSpecError::SchemaCompile("not an object".to_string());
        assert!(err.to_string().contains("not an object"));
    }
}
