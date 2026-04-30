//! Plugin marketplace — discovery, scaffold-installation, and management.
//!
//! The marketplace maintains a catalog of available plugins (both official and
//! community) and handles the local install/uninstall lifecycle.
//!
//! ## Scaffolding vs. real installation
//!
//! [`Marketplace::install`] currently performs **scaffolding only**: it creates
//! the expected directory layout and writes `manifest.toml` so that the rest of
//! the plugin system can discover and enumerate the plugin, but it does **not**
//! download a real WASM binary. A marker file (`plugin.wasm.scaffold`) is written
//! in place of a real `plugin.wasm` so that callers and the plugin loader can
//! distinguish scaffold installs from fully-functional ones.
//!
//! Real artifact downloading, signature verification, and staged installation are
//! tracked by issue #48 (WASM host) and future product work. Until then, every
//! install produces a [`ScaffoldInstall`] outcome and callers must not treat the
//! scaffold directory as a usable plugin runtime.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::builtins;

// ---------------------------------------------------------------------------
// ScaffoldInstall
// ---------------------------------------------------------------------------

/// The result of a scaffolding-only marketplace install.
///
/// This is **not** a real plugin installation — no WASM binary has been
/// downloaded. The directory structure is created so that the plugin registry
/// can enumerate the plugin, but the plugin cannot be executed until a real
/// artifact is obtained (tracked by issue #48).
#[derive(Debug, Clone)]
pub struct ScaffoldInstall {
    /// Directory that was created for this plugin.
    pub plugin_dir: PathBuf,
    /// Human-readable notice that callers should surface to users.
    pub notice: String,
}

// ---------------------------------------------------------------------------
// MarketplaceListing
// ---------------------------------------------------------------------------

/// A plugin available in the marketplace catalog.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceListing {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub downloads: u64,
    pub stars: u32,
    pub tags: Vec<String>,
    pub source_url: String,
    pub is_official: bool,
}

// ---------------------------------------------------------------------------
// Marketplace
// ---------------------------------------------------------------------------

/// Client for browsing and managing plugins.
pub struct Marketplace {
    cache: Vec<MarketplaceListing>,
    plugin_dir: PathBuf,
}

impl Default for Marketplace {
    fn default() -> Self {
        Self::new()
    }
}

impl Marketplace {
    /// Create a marketplace backed by the default plugin directory
    /// (`~/.phantom/plugins`).
    #[must_use]
    pub fn new() -> Self {
        let plugin_dir = dirs_or_default().join("plugins");
        Self {
            cache: Self::seed_catalog(),
            plugin_dir,
        }
    }

    /// Create a marketplace rooted at a specific plugin directory.
    /// Useful for testing and custom installs.
    #[must_use]
    pub fn with_dir(plugin_dir: PathBuf) -> Self {
        Self {
            cache: Self::seed_catalog(),
            plugin_dir,
        }
    }

    /// Search the catalog by name or tag. Returns all listings whose name
    /// contains `query` or that carry a matching tag. Case-insensitive.
    #[must_use]
    pub fn search(&self, query: &str) -> Vec<&MarketplaceListing> {
        let q = query.to_lowercase();
        self.cache
            .iter()
            .filter(|l| {
                l.name.to_lowercase().contains(&q)
                    || l.tags.iter().any(|t| t.to_lowercase().contains(&q))
            })
            .collect()
    }

    /// Get a listing by exact name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&MarketplaceListing> {
        self.cache.iter().find(|l| l.name == name)
    }

    /// Scaffold a plugin installation by name.
    ///
    /// This creates the expected directory layout and writes `manifest.toml` so
    /// that the plugin registry can discover the plugin. It does **not** download
    /// a real WASM binary — a marker file `plugin.wasm.scaffold` is written
    /// instead of a functional `plugin.wasm`.
    ///
    /// Callers **must** surface the [`ScaffoldInstall::notice`] to the user so
    /// it is clear that the plugin is not yet executable.
    ///
    /// Real artifact downloading is blocked on issue #48 (WASM host / wasmtime).
    pub fn install(&self, name: &str) -> Result<ScaffoldInstall> {
        let listing = match self.get(name) {
            Some(l) => l,
            None => bail!("plugin '{}' not found in marketplace", name),
        };

        let dest = self.plugin_dir.join(name);
        if dest.exists() {
            bail!(
                "plugin '{}' scaffold already exists at {}",
                name,
                dest.display()
            );
        }

        fs::create_dir_all(&dest)?;

        // Write a minimal manifest.toml so the loader can enumerate the plugin.
        // The `scaffold = true` key signals that no real WASM binary is present.
        let manifest_toml = format!(
            r#"name = "{name}"
version = "{version}"
description = "{description}"
author = "{author}"
scaffold = true
"#,
            name = listing.name,
            version = listing.version,
            description = listing.description,
            author = listing.author,
        );
        fs::write(dest.join("manifest.toml"), manifest_toml)?;

        // Write a clearly-named marker file instead of an empty plugin.wasm so
        // that no tool accidentally treats this directory as a runnable plugin.
        // Real binary installation is tracked by issue #48.
        fs::write(
            dest.join("plugin.wasm.scaffold"),
            b"# scaffold placeholder - no real WASM binary has been downloaded\n",
        )?;

        let notice = format!(
            "Scaffolded plugin '{}' at {} (placeholder only — \
             plugin is not executable until a real WASM artifact is installed; \
             see issue #48)",
            name,
            dest.display()
        );
        log::warn!("{}", notice);
        Ok(ScaffoldInstall {
            plugin_dir: dest,
            notice,
        })
    }

    /// Uninstall a plugin by removing its directory. Returns `true` if the
    /// directory existed and was removed.
    pub fn uninstall(&self, name: &str) -> Result<bool> {
        let dest = self.plugin_dir.join(name);
        if !dest.exists() {
            return Ok(false);
        }
        fs::remove_dir_all(&dest)?;
        log::info!("uninstalled plugin '{}'", name);
        Ok(true)
    }

    /// List the names of all installed plugins (directories under `plugin_dir`
    /// that contain a `manifest.toml`).
    pub fn installed(&self) -> Result<Vec<String>> {
        if !self.plugin_dir.exists() {
            return Ok(vec![]);
        }

        let mut names = Vec::new();
        for entry in fs::read_dir(&self.plugin_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir()
                && path.join("manifest.toml").exists()
                && let Some(name) = path.file_name().and_then(|n| n.to_str())
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }

    /// Build the initial catalog from official plugins and curated community
    /// contributions.
    fn seed_catalog() -> Vec<MarketplaceListing> {
        let mut catalog: Vec<MarketplaceListing> = builtins::official_plugins()
            .into_iter()
            .map(|p| MarketplaceListing {
                name: p.name,
                version: p.version,
                description: p.description,
                author: p.author,
                downloads: 0,
                stars: 0,
                tags: vec!["official".into()],
                source_url: p.homepage.unwrap_or_default(),
                is_official: true,
            })
            .collect();

        // Curated community plugins.
        catalog.extend(community_plugins());

        catalog
    }
}

// ---------------------------------------------------------------------------
// Community catalog
// ---------------------------------------------------------------------------

fn community_plugins() -> Vec<MarketplaceListing> {
    vec![
        MarketplaceListing {
            name: "pomodoro".into(),
            version: "1.0.0".into(),
            description: "Focus timer with status bar countdown and break reminders.".into(),
            author: "community".into(),
            downloads: 12_400,
            stars: 89,
            tags: vec!["productivity".into(), "timer".into(), "status-bar".into()],
            source_url: "https://github.com/phantom-plugins/pomodoro".into(),
            is_official: false,
        },
        MarketplaceListing {
            name: "crypto-ticker".into(),
            version: "0.3.2".into(),
            description: "Live cryptocurrency prices in your terminal status bar.".into(),
            author: "community".into(),
            downloads: 8_700,
            stars: 54,
            tags: vec!["finance".into(), "crypto".into(), "status-bar".into()],
            source_url: "https://github.com/phantom-plugins/crypto-ticker".into(),
            is_official: false,
        },
        MarketplaceListing {
            name: "weather-widget".into(),
            version: "0.2.1".into(),
            description: "Inline weather forecast with location detection.".into(),
            author: "community".into(),
            downloads: 6_300,
            stars: 41,
            tags: vec!["weather".into(), "status-bar".into(), "utility".into()],
            source_url: "https://github.com/phantom-plugins/weather-widget".into(),
            is_official: false,
        },
        MarketplaceListing {
            name: "clipboard-history".into(),
            version: "0.4.0".into(),
            description: "Searchable clipboard history with fuzzy matching.".into(),
            author: "community".into(),
            downloads: 15_200,
            stars: 112,
            tags: vec!["clipboard".into(), "productivity".into(), "utility".into()],
            source_url: "https://github.com/phantom-plugins/clipboard-history".into(),
            is_official: false,
        },
    ]
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the Phantom data directory. Falls back to `$HOME/.phantom` if
/// `$PHANTOM_HOME` is not set.
fn dirs_or_default() -> PathBuf {
    if let Ok(val) = std::env::var("PHANTOM_HOME") {
        return PathBuf::from(val);
    }
    if let Some(home) = home_dir() {
        return home.join(".phantom");
    }
    PathBuf::from(".phantom")
}

/// Cross-platform home directory lookup.
fn home_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_marketplace() -> (Marketplace, TempDir) {
        let tmp = TempDir::new().unwrap();
        let mp = Marketplace::with_dir(tmp.path().to_path_buf());
        (mp, tmp)
    }

    // -- Catalog --

    #[test]
    fn catalog_contains_all_official_and_community() {
        let (mp, _tmp) = test_marketplace();
        // 5 official + 4 community
        assert_eq!(mp.cache.len(), 9);
    }

    #[test]
    fn official_listings_are_marked() {
        let (mp, _tmp) = test_marketplace();
        let officials: Vec<_> = mp.cache.iter().filter(|l| l.is_official).collect();
        assert_eq!(officials.len(), 5);
        for l in &officials {
            assert!(l.tags.contains(&"official".to_string()));
        }
    }

    // -- Search --

    #[test]
    fn search_by_name_substring() {
        let (mp, _tmp) = test_marketplace();
        let results = mp.search("git");
        let names: Vec<&str> = results.iter().map(|l| l.name.as_str()).collect();
        assert!(names.contains(&"git-enhanced"));
        assert!(names.contains(&"github-notifications"));
    }

    #[test]
    fn search_by_tag() {
        let (mp, _tmp) = test_marketplace();
        let results = mp.search("productivity");
        let names: Vec<&str> = results.iter().map(|l| l.name.as_str()).collect();
        assert!(names.contains(&"pomodoro"));
        assert!(names.contains(&"clipboard-history"));
    }

    #[test]
    fn search_is_case_insensitive() {
        let (mp, _tmp) = test_marketplace();
        let results = mp.search("DOCKER");
        assert!(!results.is_empty());
        assert_eq!(results[0].name, "docker-dashboard");
    }

    #[test]
    fn search_no_match_returns_empty() {
        let (mp, _tmp) = test_marketplace();
        let results = mp.search("xyznonexistent");
        assert!(results.is_empty());
    }

    #[test]
    fn get_exact_name() {
        let (mp, _tmp) = test_marketplace();
        let listing = mp.get("spotify-controls").expect("should find spotify-controls");
        assert_eq!(listing.version, "0.1.0");
        assert!(listing.is_official);
    }

    // -- Scaffold install / Uninstall --

    #[test]
    fn scaffold_install_creates_directory_and_manifest() {
        let (mp, _tmp) = test_marketplace();
        let outcome = mp.install("pomodoro").expect("scaffold should succeed");
        assert!(outcome.plugin_dir.exists());
        assert!(outcome.plugin_dir.join("manifest.toml").exists());

        let content = fs::read_to_string(outcome.plugin_dir.join("manifest.toml")).unwrap();
        assert!(content.contains("name = \"pomodoro\""));
        assert!(
            content.contains("scaffold = true"),
            "manifest.toml must carry scaffold = true"
        );
    }

    #[test]
    fn scaffold_install_writes_marker_not_real_wasm() {
        let (mp, _tmp) = test_marketplace();
        let outcome = mp.install("pomodoro").expect("scaffold should succeed");

        // A marker file signals that no real binary was downloaded.
        assert!(
            outcome.plugin_dir.join("plugin.wasm.scaffold").exists(),
            "plugin.wasm.scaffold marker must be present"
        );
        // No empty plugin.wasm that could be mistaken for a real artifact.
        assert!(
            !outcome.plugin_dir.join("plugin.wasm").exists(),
            "plugin.wasm must NOT be written during scaffolding"
        );
    }

    #[test]
    fn scaffold_install_notice_mentions_placeholder() {
        let (mp, _tmp) = test_marketplace();
        let outcome = mp.install("pomodoro").expect("scaffold should succeed");
        assert!(
            outcome.notice.contains("placeholder") || outcome.notice.contains("scaffold"),
            "notice must clearly state the install is a scaffold/placeholder: {}",
            outcome.notice
        );
    }

    #[test]
    fn install_unknown_plugin_fails() {
        let (mp, _tmp) = test_marketplace();
        let result = mp.install("does-not-exist");
        assert!(result.is_err());
    }

    #[test]
    fn install_duplicate_fails() {
        let (mp, _tmp) = test_marketplace();
        mp.install("pomodoro").unwrap();
        let result = mp.install("pomodoro");
        assert!(result.is_err());
    }

    #[test]
    fn uninstall_removes_directory() {
        let (mp, _tmp) = test_marketplace();
        mp.install("crypto-ticker").unwrap();
        assert!(mp.uninstall("crypto-ticker").unwrap());
        assert!(!mp.plugin_dir.join("crypto-ticker").exists());
    }

    #[test]
    fn uninstall_nonexistent_returns_false() {
        let (mp, _tmp) = test_marketplace();
        assert!(!mp.uninstall("not-installed").unwrap());
    }

    // -- Installed listing --

    #[test]
    fn installed_lists_plugins() {
        let (mp, _tmp) = test_marketplace();
        mp.install("pomodoro").unwrap();
        mp.install("weather-widget").unwrap();

        let installed = mp.installed().unwrap();
        assert_eq!(installed.len(), 2);
        assert!(installed.contains(&"pomodoro".to_string()));
        assert!(installed.contains(&"weather-widget".to_string()));
    }

    #[test]
    fn installed_empty_when_no_plugins() {
        let (mp, _tmp) = test_marketplace();
        let installed = mp.installed().unwrap();
        assert!(installed.is_empty());
    }
}
