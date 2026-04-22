//! Plugin marketplace — discovery, installation, and management.
//!
//! The marketplace maintains a catalog of available plugins (both official and
//! community) and handles the local install/uninstall lifecycle. Actual WASM
//! downloads are stubbed for now; installation creates the expected directory
//! structure so the rest of the plugin system can load manifests.

use std::fs;
use std::path::PathBuf;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

use crate::builtins;

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

impl Marketplace {
    /// Create a marketplace backed by the default plugin directory
    /// (`~/.phantom/plugins`).
    pub fn new() -> Self {
        let plugin_dir = dirs_or_default().join("plugins");
        Self {
            cache: Self::seed_catalog(),
            plugin_dir,
        }
    }

    /// Create a marketplace rooted at a specific plugin directory.
    /// Useful for testing and custom installs.
    pub fn with_dir(plugin_dir: PathBuf) -> Self {
        Self {
            cache: Self::seed_catalog(),
            plugin_dir,
        }
    }

    /// Search the catalog by name or tag. Returns all listings whose name
    /// contains `query` or that carry a matching tag. Case-insensitive.
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
    pub fn get(&self, name: &str) -> Option<&MarketplaceListing> {
        self.cache.iter().find(|l| l.name == name)
    }

    /// Install a plugin by name.
    ///
    /// For now this creates the expected directory structure and writes the
    /// manifest as TOML. Actual WASM download is a future concern.
    pub fn install(&self, name: &str) -> Result<PathBuf> {
        let listing = match self.get(name) {
            Some(l) => l,
            None => bail!("plugin '{}' not found in marketplace", name),
        };

        let dest = self.plugin_dir.join(name);
        if dest.exists() {
            bail!("plugin '{}' is already installed at {}", name, dest.display());
        }

        fs::create_dir_all(&dest)?;

        // Write a minimal manifest.toml so the loader can pick it up.
        let manifest_toml = format!(
            r#"name = "{name}"
version = "{version}"
description = "{description}"
author = "{author}"
"#,
            name = listing.name,
            version = listing.version,
            description = listing.description,
            author = listing.author,
        );
        fs::write(dest.join("manifest.toml"), manifest_toml)?;

        // Placeholder for the WASM binary.
        fs::write(dest.join("plugin.wasm"), b"")?;

        log::info!("installed plugin '{}' to {}", name, dest.display());
        Ok(dest)
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
            if path.is_dir() && path.join("manifest.toml").exists() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    names.push(name.to_owned());
                }
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

    // -- Install / Uninstall --

    #[test]
    fn install_creates_directory_and_manifest() {
        let (mp, _tmp) = test_marketplace();
        let dest = mp.install("pomodoro").expect("install should succeed");
        assert!(dest.exists());
        assert!(dest.join("manifest.toml").exists());
        assert!(dest.join("plugin.wasm").exists());

        let content = fs::read_to_string(dest.join("manifest.toml")).unwrap();
        assert!(content.contains("name = \"pomodoro\""));
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
