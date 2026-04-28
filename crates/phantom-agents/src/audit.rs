//! Audit logging for agent tool calls.
//!
//! Emits one JSON record per tool call to a daily-rolling JSONL file. Records
//! carry `target = "phantom::audit"` so a downstream subscriber can filter on
//! target. Argument payloads are hashed (blake3, truncated to 16 hex chars)
//! rather than stored verbatim to keep the log compact and avoid leaking
//! secrets that may appear in args.
//!
//! # Initialization
//!
//! Call [`init`] once at startup with a writable directory; keep the returned
//! [`AuditWriter`] alive for the lifetime of the process. Dropping the guard
//! flushes the non-blocking writer's buffer.
//!
//! # Footguns
//!
//! `tracing` uses a process-global default subscriber. [`init`] uses
//! `try_init` and treats "already set" as success, so re-initialization is a
//! no-op rather than a panic. Calling [`emit_tool_call`] before [`init`] is
//! safe — the event is dispatched to whatever subscriber is currently
//! installed (usually the no-op default), so it is silently dropped.

use std::path::Path;

// ---------------------------------------------------------------------------
// AuditOutcome
// ---------------------------------------------------------------------------

/// Outcome of an audited tool call.
#[derive(Debug, Clone, Copy)]
pub enum AuditOutcome {
    Ok,
    Denied,
    Error,
}

impl AuditOutcome {
    /// Lowercase string representation, as serialized in the audit log.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AuditOutcome::Ok => "ok",
            AuditOutcome::Denied => "denied",
            AuditOutcome::Error => "error",
        }
    }
}

// ---------------------------------------------------------------------------
// AuditWriter
// ---------------------------------------------------------------------------

/// Guard that keeps the non-blocking audit writer alive. Drop to flush.
pub struct AuditWriter {
    _guard: tracing_appender::non_blocking::WorkerGuard,
}

// ---------------------------------------------------------------------------
// init
// ---------------------------------------------------------------------------

/// Initialize the audit subscriber.
///
/// Returns a guard that must be kept alive for the lifetime of the app
/// (drops flush the buffer on shutdown). Safe to call multiple times — the
/// second call returns a fresh guard but does not replace the global
/// subscriber (tracing only allows one default per process).
///
/// # Errors
///
/// Returns an `io::Error` if the audit directory cannot be created.
pub fn init(audit_dir: &Path) -> std::io::Result<AuditWriter> {
    use tracing_subscriber::fmt;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    use tracing_subscriber::Layer;

    std::fs::create_dir_all(audit_dir)?;

    let appender = tracing_appender::rolling::daily(audit_dir, "audit.jsonl");
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);

    let layer = fmt::layer()
        .json()
        .with_target(true)
        .with_level(false)
        .with_current_span(false)
        .with_span_list(false)
        .with_writer(non_blocking)
        .with_filter(tracing_subscriber::filter::Targets::new().with_target(
            "phantom::audit",
            tracing::Level::INFO,
        ));

    // try_init returns Err if a global subscriber is already set; that's fine
    // — we still hand back the guard so the caller can keep our writer alive.
    let _ = tracing_subscriber::registry().with(layer).try_init();

    Ok(AuditWriter { _guard: guard })
}

// ---------------------------------------------------------------------------
// emit_tool_call
// ---------------------------------------------------------------------------

/// Hash `args_json` with blake3, return the first 16 hex chars.
fn hash_args(args_json: &str) -> String {
    let mut hex = blake3::hash(args_json.as_bytes()).to_hex().to_string();
    hex.truncate(16);
    hex
}

/// Current time as RFC3339 (best-effort; falls back to seconds since epoch).
fn now_ts() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // We don't pull chrono in just for this; emit ms-since-epoch as a string.
    format!("{}.{:03}", dur.as_secs(), dur.subsec_millis())
}

/// Emit a single audit record for a tool call.
///
/// Safe to call before [`init`] — the event is dispatched to the global
/// subscriber, which is a no-op until `init` installs the audit layer.
pub fn emit_tool_call(
    agent_id: u64,
    role: &str,
    class: &str,
    tool: &str,
    args_json: &str,
    outcome: AuditOutcome,
) {
    let args_hash = hash_args(args_json);
    let ts = now_ts();
    tracing::event!(
        target: "phantom::audit",
        tracing::Level::INFO,
        ts = %ts,
        agent_id = agent_id,
        role = role,
        class = class,
        tool = tool,
        args_hash = %args_hash,
        outcome = outcome.as_str(),
    );
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_serializes_lowercase() {
        assert_eq!(AuditOutcome::Ok.as_str(), "ok");
        assert_eq!(AuditOutcome::Denied.as_str(), "denied");
        assert_eq!(AuditOutcome::Error.as_str(), "error");
    }

    #[test]
    fn args_hash_is_deterministic() {
        let a = hash_args(r#"{"path":"/etc/passwd"}"#);
        let b = hash_args(r#"{"path":"/etc/passwd"}"#);
        let c = hash_args(r#"{"path":"/etc/hosts"}"#);
        assert_eq!(a, b, "identical args must hash identically");
        assert_ne!(a, c, "different args must hash differently");
        assert_eq!(a.len(), 16, "hash must be truncated to 16 hex chars");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "hash must be hex"
        );
    }

    #[test]
    fn emit_before_init_does_not_panic() {
        // No init called by *this* test; emit must not panic regardless of
        // whether some other test happens to have installed a global
        // subscriber first. (See module footgun docs.)
        emit_tool_call(
            42,
            "noop-role",
            "Sense",
            "noop-tool",
            r#"{"x":1}"#,
            AuditOutcome::Ok,
        );
    }

    /// Single combined test that exercises init -> emit -> drop -> file.
    ///
    /// Why one test instead of two: tracing's default subscriber is
    /// process-global and `try_init` is winner-takes-all. If we ran a
    /// separate `init_succeeds_in_writable_temp_dir` test it would race
    /// this one for the global subscriber slot — whichever lost would
    /// emit into the winner's (already-dropped) tempdir, producing a
    /// flaky empty-file failure. Combined, both invariants are checked
    /// deterministically: init returns Ok and a record actually lands.
    #[test]
    fn init_then_emit_writes_record_and_drop_flushes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // init must succeed on a writable temp dir and return a guard.
        let writer = init(tmp.path()).expect("init must succeed");

        let agent_id: u64 = 0xDEAD_BEEF_CAFE_F00D;
        let tool = "audit-test-tool-unique-marker";
        emit_tool_call(
            agent_id,
            "tester",
            "Sense",
            tool,
            r#"{"hello":"world"}"#,
            AuditOutcome::Ok,
        );

        // Dropping the guard flushes the non-blocking writer's buffer.
        drop(writer);

        // tracing-appender names rolling files `audit.jsonl.YYYY-MM-DD`.
        // Concatenate every file in the dir whose name starts with that
        // prefix, then look for our marker line.
        let entries = std::fs::read_dir(tmp.path()).expect("readdir");
        let mut all = String::new();
        let mut any_audit_file = false;
        for entry in entries {
            let entry = entry.expect("dirent");
            let name = entry.file_name();
            let name = name.to_string_lossy().into_owned();
            if !name.starts_with("audit.jsonl") {
                continue;
            }
            any_audit_file = true;
            let contents = std::fs::read_to_string(entry.path()).expect("read");
            all.push_str(&contents);
        }
        assert!(
            any_audit_file,
            "a daily-rolling audit.jsonl* file must exist after emit+drop"
        );

        let needle_id = format!("{agent_id}");
        let line = all
            .lines()
            .find(|l| l.contains(&needle_id) && l.contains(tool))
            .expect("a JSONL line containing our agent_id and tool")
            .to_string();

        let v: serde_json::Value =
            serde_json::from_str(&line).expect("audit line must be valid JSON");
        // tracing-subscriber's json formatter nests user fields under `fields`.
        let fields = v.get("fields").unwrap_or(&v);
        assert_eq!(fields.get("tool").and_then(|x| x.as_str()), Some(tool));
        assert_eq!(fields.get("outcome").and_then(|x| x.as_str()), Some("ok"));
        let hash = fields
            .get("args_hash")
            .and_then(|x| x.as_str())
            .expect("args_hash field");
        assert_eq!(hash.len(), 16, "args_hash must be 16 hex chars");
        assert_eq!(
            v.get("target").and_then(|x| x.as_str()),
            Some("phantom::audit")
        );
    }
}
