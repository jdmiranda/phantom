//! Per-agent lifecycle journal helpers.

use log::warn;

/// Open (or create) the per-agent JSONL journal file.
///
/// Returns `None` on any I/O error; callers treat the journal as best-effort
/// observability and never abort an agent spawn on journal failure.
pub(super) fn open_agent_journal(
    agent_id: u64,
) -> Option<phantom_memory::journal::AgentJournal> {
    let dir = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".config/phantom/agents/journals"))
        .unwrap_or_else(|| std::env::temp_dir().join("phantom-agents/journals"));
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!(
            "AgentJournal: could not create journal dir {}: {e}",
            dir.display()
        );
        return None;
    }
    let path = dir.join(format!("{agent_id}.jsonl"));
    match phantom_memory::journal::AgentJournal::open(&path) {
        Ok(j) => Some(j),
        Err(e) => {
            warn!("AgentJournal: could not open {}: {e}", path.display());
            None
        }
    }
}
