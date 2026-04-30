//! Startup and runtime capability audit for offline readiness (Issue #362).
//!
//! Computes a [`CapabilityReport`] that classifies every major subsystem into
//! one of three states:
//!
//! - [`CapabilityStatus::Available`] — subsystem is functional right now.
//! - [`CapabilityStatus::BlockedByPolicy`] — subsystem is intentionally
//!   disabled by an application-level policy (e.g. privacy mode).
//! - [`CapabilityStatus::Unavailable`] — subsystem cannot be reached or is
//!   misconfigured (e.g. Ollama not running, API key missing).
//!
//! # Design intent
//!
//! The report is a **single truth source** so the app, the inspector, and the
//! status bar all agree on what works right now.  It deliberately avoids
//! making cloud network calls: cloud backend probing is limited to checking
//! whether the API key env-var is set.  Only the local Ollama backend
//! performs an actual TCP ping (to `localhost:11434`).
//!
//! # Privacy-mode interaction
//!
//! When `privacy_mode` is `true`, cloud backends are classified as
//! [`CapabilityStatus::BlockedByPolicy`] **without** making any network call —
//! the policy check happens before the reachability check.
//!
//! # Usage
//!
//! ```rust,no_run
//! use phantom_brain::capability_audit::{AuditConfig, CapabilityReport};
//!
//! let config = AuditConfig { privacy_mode: false };
//! let report = CapabilityReport::compute(&config);
//! for entry in report.entries() {
//!     log::info!("[capability] {} — {:?}", entry.name, entry.status);
//! }
//! ```

// ---------------------------------------------------------------------------
// CapabilityStatus
// ---------------------------------------------------------------------------

/// Classification of a single capability at audit time.
///
/// The three variants map directly onto the issue acceptance criteria:
/// - `Available`        → subsystem is online and usable.
/// - `BlockedByPolicy` → subsystem would work but a policy prevents it.
/// - `Unavailable`     → subsystem cannot be reached or is misconfigured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityStatus {
    /// The subsystem is functional.
    Available,
    /// The subsystem is intentionally disabled by application policy.
    ///
    /// The canonical example is a cloud backend when privacy mode is on.
    /// Treat this as *by design*, not as an error.
    BlockedByPolicy,
    /// The subsystem cannot be used: missing credentials, process not running,
    /// or misconfigured endpoint.
    ///
    /// The `reason` field on [`CapabilityAuditEntry`] carries a human-readable
    /// explanation.
    Unavailable,
}

// ---------------------------------------------------------------------------
// CapabilityAuditEntry
// ---------------------------------------------------------------------------

/// A single entry in the capability audit report.
#[derive(Debug, Clone)]
pub struct CapabilityAuditEntry {
    /// Short human-readable name, e.g. `"core-terminal"`.
    pub name: &'static str,
    /// Status of this capability at audit time.
    pub status: CapabilityStatus,
    /// Optional human-readable explanation, especially for
    /// [`CapabilityStatus::Unavailable`] entries.
    pub reason: Option<String>,
}

impl CapabilityAuditEntry {
    fn available(name: &'static str) -> Self {
        Self {
            name,
            status: CapabilityStatus::Available,
            reason: None,
        }
    }

    fn blocked(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            status: CapabilityStatus::BlockedByPolicy,
            reason: Some(reason.into()),
        }
    }

    fn unavailable(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            status: CapabilityStatus::Unavailable,
            reason: Some(reason.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// AuditConfig
// ---------------------------------------------------------------------------

/// Input configuration for a capability audit run.
#[derive(Debug, Clone)]
pub struct AuditConfig {
    /// Mirror of `PhantomConfig::privacy_mode`.
    ///
    /// When `true`, all cloud backends are classified as
    /// [`CapabilityStatus::BlockedByPolicy`] without performing any network
    /// call.
    pub privacy_mode: bool,
}

// ---------------------------------------------------------------------------
// CapabilityReport
// ---------------------------------------------------------------------------

/// The full capability audit report.
///
/// Constructed by [`CapabilityReport::compute`]. The entries are in a stable
/// order: core terminal, then AI backends (heuristic, Ollama, Claude), then
/// plugin runtime.
#[derive(Debug, Clone)]
pub struct CapabilityReport {
    entries: Vec<CapabilityAuditEntry>,
}

impl CapabilityReport {
    /// Run the capability audit and return a report.
    ///
    /// This function is **synchronous** and may block briefly (up to 2 s) on
    /// the Ollama TCP ping. It must not be called on the GPU/render thread.
    /// Call it on the brain thread or from a dedicated startup task.
    pub fn compute(config: &AuditConfig) -> Self {
        let mut entries = Vec::with_capacity(6);

        // 1. Core terminal — always available; the VT emulator requires no
        //    external resources.
        entries.push(CapabilityAuditEntry::available("core-terminal"));

        // 2. Heuristic (rule-based) brain — always available; no network.
        entries.push(CapabilityAuditEntry::available("heuristic-brain"));

        // 3. Ollama local backend.
        entries.push(audit_ollama());

        // 4. Claude cloud backend.
        entries.push(audit_claude(config.privacy_mode));

        // 5. OpenAI-compatible endpoint (generic cloud).
        entries.push(audit_openai_compat(config.privacy_mode));

        // 6. Plugin runtime (WASM host).
        entries.push(audit_plugin_runtime());

        Self { entries }
    }

    /// Iterate over all audit entries.
    pub fn entries(&self) -> &[CapabilityAuditEntry] {
        &self.entries
    }

    /// Returns `true` if every entry is [`CapabilityStatus::Available`].
    ///
    /// Note: `BlockedByPolicy` entries count as **not** fully available —
    /// use [`Self::all_online_or_blocked`] if you want to treat policy-blocked
    /// entries as acceptable.
    pub fn all_available(&self) -> bool {
        self.entries
            .iter()
            .all(|e| e.status == CapabilityStatus::Available)
    }

    /// Returns `true` if no entry is [`CapabilityStatus::Unavailable`].
    ///
    /// Treats `BlockedByPolicy` as an acceptable state (the user asked for it).
    pub fn all_online_or_blocked(&self) -> bool {
        self.entries
            .iter()
            .all(|e| e.status != CapabilityStatus::Unavailable)
    }

    /// Collect all entries whose status is [`CapabilityStatus::Unavailable`].
    pub fn unavailable_entries(&self) -> Vec<&CapabilityAuditEntry> {
        self.entries
            .iter()
            .filter(|e| e.status == CapabilityStatus::Unavailable)
            .collect()
    }

    /// Log the full report at INFO level.
    ///
    /// One log line per entry:
    /// `[capability-audit] <name> — <status> [: <reason>]`
    pub fn log_report(&self) {
        for entry in &self.entries {
            let status_str = match entry.status {
                CapabilityStatus::Available => "available",
                CapabilityStatus::BlockedByPolicy => "blocked-by-policy",
                CapabilityStatus::Unavailable => "unavailable",
            };
            if let Some(reason) = &entry.reason {
                log::info!(
                    "[capability-audit] {} — {} : {}",
                    entry.name,
                    status_str,
                    reason
                );
            } else {
                log::info!("[capability-audit] {} — {}", entry.name, status_str);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Private probe helpers
// ---------------------------------------------------------------------------

/// Probe the Ollama local backend (localhost:11434/api/tags, 2 s timeout).
fn audit_ollama() -> CapabilityAuditEntry {
    if crate::ollama::is_available() {
        CapabilityAuditEntry::available("ollama-local")
    } else {
        CapabilityAuditEntry::unavailable(
            "ollama-local",
            "Ollama daemon not reachable at localhost:11434 — start Ollama or configure a different endpoint",
        )
    }
}

/// Probe the Anthropic Claude cloud backend.
///
/// No network call is made — availability is determined by the presence of
/// `ANTHROPIC_API_KEY` in the environment. When `privacy_mode` is `true` the
/// entry is `BlockedByPolicy` regardless of the key.
fn audit_claude(privacy_mode: bool) -> CapabilityAuditEntry {
    if privacy_mode {
        return CapabilityAuditEntry::blocked(
            "claude-cloud",
            "privacy mode is active — cloud API calls are disabled",
        );
    }
    if std::env::var("ANTHROPIC_API_KEY").is_ok() {
        CapabilityAuditEntry::available("claude-cloud")
    } else {
        CapabilityAuditEntry::unavailable(
            "claude-cloud",
            "ANTHROPIC_API_KEY is not set — Claude API calls will fail",
        )
    }
}

/// Probe a generic OpenAI-compatible endpoint.
///
/// Availability is determined by the presence of `OPENAI_API_KEY` (or
/// `OPENAI_BASE_URL` for local-compat endpoints). When `privacy_mode` is
/// `true` the entry is `BlockedByPolicy` unconditionally.
fn audit_openai_compat(privacy_mode: bool) -> CapabilityAuditEntry {
    if privacy_mode {
        return CapabilityAuditEntry::blocked(
            "openai-compat-cloud",
            "privacy mode is active — cloud API calls are disabled",
        );
    }
    let key_set = std::env::var("OPENAI_API_KEY").is_ok();
    let base_url_set = std::env::var("OPENAI_BASE_URL").is_ok();
    if key_set || base_url_set {
        CapabilityAuditEntry::available("openai-compat-cloud")
    } else {
        CapabilityAuditEntry::unavailable(
            "openai-compat-cloud",
            "neither OPENAI_API_KEY nor OPENAI_BASE_URL is set",
        )
    }
}

/// Probe the WASM plugin runtime.
///
/// The mock WASM host is always present; the real `wasmtime` host requires
/// the `wasmtime` feature (not yet wired — Issue #48).  We report the mock
/// as available at reduced capability.
fn audit_plugin_runtime() -> CapabilityAuditEntry {
    // The mock runtime is always compiled in. The real wasmtime host is
    // not yet wired (Issue #48), so we mark the mock as available and note
    // that full WASM sandboxing is pending.
    CapabilityAuditEntry {
        name: "plugin-runtime",
        status: CapabilityStatus::Available,
        reason: Some(
            "mock WASM host active — real wasmtime sandboxing pending (Issue #48)".into(),
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn open_config() -> AuditConfig {
        AuditConfig { privacy_mode: false }
    }

    // -----------------------------------------------------------------------
    // CapabilityStatus equality
    // -----------------------------------------------------------------------

    #[test]
    fn status_equality() {
        assert_eq!(CapabilityStatus::Available, CapabilityStatus::Available);
        assert_ne!(CapabilityStatus::Available, CapabilityStatus::Unavailable);
        assert_ne!(
            CapabilityStatus::BlockedByPolicy,
            CapabilityStatus::Unavailable
        );
    }

    // -----------------------------------------------------------------------
    // Entry constructors
    // -----------------------------------------------------------------------

    #[test]
    fn available_entry_has_no_reason() {
        let entry = CapabilityAuditEntry::available("core-terminal");
        assert_eq!(entry.name, "core-terminal");
        assert_eq!(entry.status, CapabilityStatus::Available);
        assert!(entry.reason.is_none());
    }

    #[test]
    fn blocked_entry_carries_reason() {
        let entry = CapabilityAuditEntry::blocked("claude-cloud", "privacy mode");
        assert_eq!(entry.status, CapabilityStatus::BlockedByPolicy);
        assert_eq!(entry.reason.as_deref(), Some("privacy mode"));
    }

    #[test]
    fn unavailable_entry_carries_reason() {
        let entry = CapabilityAuditEntry::unavailable("ollama-local", "not running");
        assert_eq!(entry.status, CapabilityStatus::Unavailable);
        assert!(entry.reason.is_some());
    }

    // -----------------------------------------------------------------------
    // Privacy mode blocks cloud backends without network calls
    // -----------------------------------------------------------------------

    #[test]
    fn claude_blocked_when_privacy_on() {
        let entry = audit_claude(true);
        assert_eq!(entry.status, CapabilityStatus::BlockedByPolicy);
        assert_eq!(entry.name, "claude-cloud");
    }

    #[test]
    fn openai_blocked_when_privacy_on() {
        let entry = audit_openai_compat(true);
        assert_eq!(entry.status, CapabilityStatus::BlockedByPolicy);
        assert_eq!(entry.name, "openai-compat-cloud");
    }

    // -----------------------------------------------------------------------
    // Claude availability depends on env var (without privacy mode)
    // -----------------------------------------------------------------------

    #[test]
    fn claude_unavailable_when_key_missing() {
        // Remove key for the duration of this test.
        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };

        let entry = audit_claude(false);
        assert_eq!(entry.status, CapabilityStatus::Unavailable);

        // Restore.
        if let Some(k) = prev {
            unsafe { std::env::set_var("ANTHROPIC_API_KEY", k) };
        }
    }

    #[test]
    fn claude_available_when_key_present() {
        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "sk-test") };

        let entry = audit_claude(false);
        assert_eq!(entry.status, CapabilityStatus::Available);

        // Restore.
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };
        if let Some(k) = prev {
            unsafe { std::env::set_var("ANTHROPIC_API_KEY", k) };
        }
    }

    // -----------------------------------------------------------------------
    // OpenAI-compat: available when either key or base_url is set
    // -----------------------------------------------------------------------

    #[test]
    fn openai_available_when_key_set() {
        let prev_key = std::env::var("OPENAI_API_KEY").ok();
        let prev_url = std::env::var("OPENAI_BASE_URL").ok();
        unsafe {
            std::env::remove_var("OPENAI_BASE_URL");
            std::env::set_var("OPENAI_API_KEY", "test-key");
        }

        let entry = audit_openai_compat(false);
        assert_eq!(entry.status, CapabilityStatus::Available);

        unsafe { std::env::remove_var("OPENAI_API_KEY") };
        if let Some(k) = prev_key {
            unsafe { std::env::set_var("OPENAI_API_KEY", k) };
        }
        if let Some(u) = prev_url {
            unsafe { std::env::set_var("OPENAI_BASE_URL", u) };
        }
    }

    #[test]
    fn openai_available_when_base_url_set() {
        let prev_key = std::env::var("OPENAI_API_KEY").ok();
        let prev_url = std::env::var("OPENAI_BASE_URL").ok();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::set_var("OPENAI_BASE_URL", "http://localhost:8080");
        }

        let entry = audit_openai_compat(false);
        assert_eq!(entry.status, CapabilityStatus::Available);

        unsafe { std::env::remove_var("OPENAI_BASE_URL") };
        if let Some(k) = prev_key {
            unsafe { std::env::set_var("OPENAI_API_KEY", k) };
        }
        if let Some(u) = prev_url {
            unsafe { std::env::set_var("OPENAI_BASE_URL", u) };
        }
    }

    #[test]
    fn openai_unavailable_when_neither_set() {
        let prev_key = std::env::var("OPENAI_API_KEY").ok();
        let prev_url = std::env::var("OPENAI_BASE_URL").ok();
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
            std::env::remove_var("OPENAI_BASE_URL");
        }

        let entry = audit_openai_compat(false);
        assert_eq!(entry.status, CapabilityStatus::Unavailable);

        if let Some(k) = prev_key {
            unsafe { std::env::set_var("OPENAI_API_KEY", k) };
        }
        if let Some(u) = prev_url {
            unsafe { std::env::set_var("OPENAI_BASE_URL", u) };
        }
    }

    // -----------------------------------------------------------------------
    // Plugin runtime is always available (mock host)
    // -----------------------------------------------------------------------

    #[test]
    fn plugin_runtime_always_available() {
        let entry = audit_plugin_runtime();
        assert_eq!(entry.status, CapabilityStatus::Available);
        assert_eq!(entry.name, "plugin-runtime");
    }

    // -----------------------------------------------------------------------
    // CapabilityReport shape
    // -----------------------------------------------------------------------

    #[test]
    fn report_has_expected_entries() {
        let config = open_config();
        // Don't call compute() — it would block on Ollama ping.
        // Test the individual probes instead. Verify entry count via
        // manual construction.
        let entries = vec![
            CapabilityAuditEntry::available("core-terminal"),
            CapabilityAuditEntry::available("heuristic-brain"),
            CapabilityAuditEntry::unavailable("ollama-local", "not running"),
            CapabilityAuditEntry::unavailable("claude-cloud", "no key"),
            CapabilityAuditEntry::unavailable("openai-compat-cloud", "no key"),
            CapabilityAuditEntry::available("plugin-runtime"),
        ];
        let report = CapabilityReport { entries };

        assert_eq!(report.entries().len(), 6);
        assert!(!report.all_available(), "some entries are unavailable");
        assert!(
            !report.all_online_or_blocked(),
            "unavailable entries must not count as blocked"
        );
        assert_eq!(report.unavailable_entries().len(), 3);
        let _ = config; // silence unused warning
    }

    #[test]
    fn report_all_available_when_all_entries_available() {
        let entries = vec![
            CapabilityAuditEntry::available("a"),
            CapabilityAuditEntry::available("b"),
        ];
        let report = CapabilityReport { entries };
        assert!(report.all_available());
        assert!(report.all_online_or_blocked());
        assert!(report.unavailable_entries().is_empty());
    }

    #[test]
    fn report_blocked_not_counted_as_unavailable() {
        let entries = vec![
            CapabilityAuditEntry::available("core-terminal"),
            CapabilityAuditEntry::blocked("claude-cloud", "privacy mode"),
        ];
        let report = CapabilityReport { entries };
        assert!(!report.all_available(), "blocked entries lower all_available");
        assert!(report.all_online_or_blocked());
        assert!(report.unavailable_entries().is_empty());
    }

    // -----------------------------------------------------------------------
    // Privacy config blocks both cloud entries
    // -----------------------------------------------------------------------

    #[test]
    fn privacy_mode_blocks_both_cloud_entries() {
        // Build the report manually to avoid the Ollama TCP ping.
        let entries = vec![
            CapabilityAuditEntry::available("core-terminal"),
            CapabilityAuditEntry::available("heuristic-brain"),
            CapabilityAuditEntry::unavailable("ollama-local", "not running"),
            audit_claude(true),
            audit_openai_compat(true),
            audit_plugin_runtime(),
        ];
        let report = CapabilityReport { entries };

        let blocked: Vec<_> = report
            .entries()
            .iter()
            .filter(|e| e.status == CapabilityStatus::BlockedByPolicy)
            .collect();
        assert_eq!(blocked.len(), 2, "both cloud entries should be blocked");
        assert!(blocked.iter().all(|e| e.reason.is_some()));
    }
}
