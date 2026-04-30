//! Persistence helpers for the per-peer capability grant registry (issue #8).
//!
//! Grants are stored as a JSON file at
//! `~/.config/phantom/peer_grants.json` (or `$XDG_CONFIG_HOME/phantom/…`)
//! so they survive Phantom restarts.  On boot the registry is loaded from
//! disk; on every mutation it is written back.
//!
//! # File format
//!
//! ```json
//! [
//!   {
//!     "peer_id": "abc-123",
//!     "allowed_classes": ["Sense", "Coordinate"],
//!     "expires_at_secs": null
//!   }
//! ]
//! ```
//!
//! `expires_at_secs` is a Unix timestamp (seconds since the UNIX epoch) or
//! `null` for permanent grants.  Expired entries are silently dropped on load.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use log::{debug, warn};
use serde::{Deserialize, Serialize};

use phantom_agents::{PeerGrantRegistry, PeerGrants, role::CapabilityClass};
use phantom_agents::PeerId;

// ---------------------------------------------------------------------------
// Serializable grant record
// ---------------------------------------------------------------------------

/// Serializable form of a peer grant entry.
///
/// Uses a Unix timestamp for expiry so the value survives process restarts
/// (unlike `std::time::Instant`, which is monotonic and process-local).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct GrantRecord {
    peer_id: String,
    allowed_classes: Vec<String>,
    /// Unix timestamp (seconds) after which the grant expires, or `null`.
    expires_at_secs: Option<u64>,
}

impl GrantRecord {
    fn class_from_str(s: &str) -> Option<CapabilityClass> {
        match s {
            "Sense" => Some(CapabilityClass::Sense),
            "Coordinate" => Some(CapabilityClass::Coordinate),
            "Act" => Some(CapabilityClass::Act),
            "Reflect" => Some(CapabilityClass::Reflect),
            "Compute" => Some(CapabilityClass::Compute),
            _ => None,
        }
    }

    fn class_to_str(c: CapabilityClass) -> &'static str {
        match c {
            CapabilityClass::Sense => "Sense",
            CapabilityClass::Coordinate => "Coordinate",
            CapabilityClass::Act => "Act",
            CapabilityClass::Reflect => "Reflect",
            CapabilityClass::Compute => "Compute",
        }
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Load the peer grant registry from disk, or return an empty registry if the
/// file does not exist or cannot be parsed.
///
/// Expired entries (where `expires_at_secs` is in the past) are silently
/// dropped so the registry is always in a valid state on boot.
pub fn load_peer_grant_registry() -> PeerGrantRegistry {
    load_from_path(&grants_path())
}

fn load_from_path(path: &std::path::Path) -> PeerGrantRegistry {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => {
            debug!("peer_grants: no grants file at {}", path.display());
            return PeerGrantRegistry::new();
        }
    };

    let records: Vec<GrantRecord> = match serde_json::from_str(&content) {
        Ok(r) => r,
        Err(e) => {
            warn!("peer_grants: failed to parse {}: {e}", path.display());
            return PeerGrantRegistry::new();
        }
    };

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut registry = PeerGrantRegistry::new();
    for record in records {
        // Drop expired entries.
        if let Some(exp) = record.expires_at_secs {
            if exp <= now_secs {
                debug!(
                    "peer_grants: skipping expired grant for {} (expired {}s ago)",
                    record.peer_id,
                    now_secs.saturating_sub(exp)
                );
                continue;
            }
        }

        let classes: HashSet<CapabilityClass> = record
            .allowed_classes
            .iter()
            .filter_map(|s| GrantRecord::class_from_str(s))
            .collect();

        let until = record.expires_at_secs.map(|secs| {
            // Convert Unix timestamp back to an Instant by computing the
            // remaining duration from now.
            let remaining = secs.saturating_sub(now_secs);
            Instant::now() + Duration::from_secs(remaining)
        });

        registry.grant(PeerId::new(&record.peer_id), classes, until);
    }

    let count = registry.iter().count();
    debug!(
        "peer_grants: loaded {} grant(s) from {}",
        count,
        path.display()
    );
    registry
}

/// Persist the current registry to disk.
///
/// Best-effort: failures are logged and silently swallowed so a disk error
/// does not crash the application.
pub fn save_peer_grant_registry(registry: &PeerGrantRegistry) {
    save_to_path(registry, &grants_path());
}

fn save_to_path(registry: &PeerGrantRegistry, path: &std::path::Path) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!("peer_grants: cannot create config dir: {e}");
            return;
        }
    }

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let records: Vec<GrantRecord> = registry
        .iter()
        .map(|g: &PeerGrants| {
            let allowed_classes = g
                .allowed_classes
                .iter()
                .map(|c| GrantRecord::class_to_str(*c).to_string())
                .collect();

            let expires_at_secs = g.until.map(|t| {
                let remaining = t
                    .checked_duration_since(Instant::now())
                    .unwrap_or(Duration::ZERO);
                now_secs + remaining.as_secs()
            });

            GrantRecord {
                peer_id: g.peer_id.to_string(),
                allowed_classes,
                expires_at_secs,
            }
        })
        .collect();

    match serde_json::to_string_pretty(&records) {
        Ok(json) => {
            if let Err(e) = std::fs::write(path, json) {
                warn!("peer_grants: failed to write {}: {e}", path.display());
            } else {
                debug!(
                    "peer_grants: saved {} grant(s) to {}",
                    records.len(),
                    path.display()
                );
            }
        }
        Err(e) => {
            warn!("peer_grants: serialization error: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Path helper
// ---------------------------------------------------------------------------

fn grants_path() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
            .join("phantom")
            .join("peer_grants.json")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home)
            .join(".config")
            .join("phantom")
            .join("peer_grants.json")
    } else {
        PathBuf::from("peer_grants.json")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Returns a fresh temporary directory and a path within it for the grants
    /// file. Tests use `load_from_path` / `save_to_path` directly so that no
    /// global env-var mutation is required (env-var writes are not safe under
    /// parallel test execution).
    fn temp_grants_path() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("peer_grants.json");
        (dir, path)
    }

    #[test]
    fn empty_registry_returns_new() {
        let (dir, path) = temp_grants_path();
        let registry = load_from_path(&path);
        assert_eq!(registry.iter().count(), 0);
        drop(dir);
    }

    #[test]
    fn save_and_reload_grants() {
        let (dir, path) = temp_grants_path();
        let mut registry = PeerGrantRegistry::new();
        registry.grant(
            PeerId::new("peer-A"),
            HashSet::from([CapabilityClass::Sense, CapabilityClass::Coordinate]),
            None, // permanent
        );
        registry.grant(
            PeerId::new("peer-B"),
            HashSet::from([CapabilityClass::Act]),
            None,
        );

        save_to_path(&registry, &path);

        let reloaded = load_from_path(&path);
        assert!(reloaded.check(&PeerId::new("peer-A"), CapabilityClass::Sense));
        assert!(reloaded.check(&PeerId::new("peer-A"), CapabilityClass::Coordinate));
        assert!(!reloaded.check(&PeerId::new("peer-A"), CapabilityClass::Act));
        assert!(reloaded.check(&PeerId::new("peer-B"), CapabilityClass::Act));
        drop(dir);
    }

    #[test]
    fn expired_grants_are_dropped_on_load() {
        let (dir, path) = temp_grants_path();
        // Write a grant record with an already-expired timestamp.
        // Use epoch 1 (far in the past) as the expiry.
        let json =
            r#"[{"peer_id":"old-peer","allowed_classes":["Sense"],"expires_at_secs":1}]"#;
        std::fs::write(&path, json).unwrap();

        let registry = load_from_path(&path);
        assert_eq!(
            registry.iter().count(),
            0,
            "expired grant should not be loaded"
        );
        assert!(!registry.check(&PeerId::new("old-peer"), CapabilityClass::Sense));
        drop(dir);
    }

    #[test]
    fn revoke_then_save_removes_entry() {
        let (dir, path) = temp_grants_path();
        let mut registry = PeerGrantRegistry::new();
        registry.grant(
            PeerId::new("temp-peer"),
            HashSet::from([CapabilityClass::Sense]),
            None,
        );
        save_to_path(&registry, &path);

        // Revoke and save.
        registry.revoke(&PeerId::new("temp-peer"));
        save_to_path(&registry, &path);

        let reloaded = load_from_path(&path);
        assert!(!reloaded.check(&PeerId::new("temp-peer"), CapabilityClass::Sense));
        drop(dir);
    }
}
