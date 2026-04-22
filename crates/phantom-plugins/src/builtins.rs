//! Official built-in plugin manifests.
//!
//! These define the metadata for every plugin that ships with Phantom. The
//! actual implementations are WASM modules loaded at runtime — the manifests
//! here serve as the authoritative catalog and as documentation for the plugin
//! system.

use crate::manifest::{
    CommandDef, HookType, Permission, PluginManifest, StatusBarDef, StatusBarPosition,
};

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// All official plugin manifests, ordered by name.
pub fn official_plugins() -> Vec<PluginManifest> {
    vec![
        git_enhanced(),
        docker_dashboard(),
        api_inspector(),
        spotify_controls(),
        github_notifications(),
    ]
}

/// Look up an official plugin by exact name.
pub fn get_official(name: &str) -> Option<PluginManifest> {
    official_plugins().into_iter().find(|p| p.name == name)
}

// ---------------------------------------------------------------------------
// git-enhanced
// ---------------------------------------------------------------------------

fn git_enhanced() -> PluginManifest {
    PluginManifest {
        name: "git-enhanced".into(),
        version: "0.1.0".into(),
        description: "Rich git integration — interactive commit graph and PR review helper."
            .into(),
        author: "Phantom Team".into(),
        license: Some("MIT".into()),
        homepage: Some("https://github.com/jdmiranda/phantom/tree/main/plugins/git-enhanced".into()),
        entry_point: "git_enhanced.wasm".into(),
        permissions: vec![Permission::ReadFiles, Permission::RunCommands],
        hooks: vec![HookType::OnCommand("git *".into())],
        commands: vec![
            CommandDef {
                name: "git-graph".into(),
                description: "Interactive commit graph with branch topology.".into(),
                usage: "git-graph [--all] [--since <date>]".into(),
            },
            CommandDef {
                name: "git-review".into(),
                description: "AI-assisted pull request review.".into(),
                usage: "git-review [<pr-number>]".into(),
            },
        ],
        status_bar: None,
    }
}

// ---------------------------------------------------------------------------
// docker-dashboard
// ---------------------------------------------------------------------------

fn docker_dashboard() -> PluginManifest {
    PluginManifest {
        name: "docker-dashboard".into(),
        version: "0.1.0".into(),
        description: "Live container dashboard with streaming logs viewer.".into(),
        author: "Phantom Team".into(),
        license: Some("MIT".into()),
        homepage: Some("https://github.com/jdmiranda/phantom/tree/main/plugins/docker-dashboard".into()),
        entry_point: "docker_dashboard.wasm".into(),
        permissions: vec![Permission::RunCommands, Permission::StatusBar],
        hooks: vec![HookType::OnCommand("docker *".into())],
        commands: vec![
            CommandDef {
                name: "docker-dash".into(),
                description: "Live container dashboard with resource usage.".into(),
                usage: "docker-dash [--watch]".into(),
            },
            CommandDef {
                name: "docker-logs".into(),
                description: "Streaming container log viewer with filtering.".into(),
                usage: "docker-logs <container> [--follow] [--filter <pattern>]".into(),
            },
        ],
        status_bar: Some(StatusBarDef {
            position: StatusBarPosition::Right,
            update_interval_ms: 5000,
        }),
    }
}

// ---------------------------------------------------------------------------
// api-inspector
// ---------------------------------------------------------------------------

fn api_inspector() -> PluginManifest {
    PluginManifest {
        name: "api-inspector".into(),
        version: "0.1.0".into(),
        description:
            "HTTP response detection, auto-formatted JSON, status codes, and request builder."
                .into(),
        author: "Phantom Team".into(),
        license: Some("MIT".into()),
        homepage: Some("https://github.com/jdmiranda/phantom/tree/main/plugins/api-inspector".into()),
        entry_point: "api_inspector.wasm".into(),
        permissions: vec![Permission::Network],
        hooks: vec![HookType::OnOutput],
        commands: vec![CommandDef {
            name: "api".into(),
            description: "Interactive HTTP request builder with history.".into(),
            usage: "api <method> <url> [--header <k:v>] [--body <json>]".into(),
        }],
        status_bar: None,
    }
}

// ---------------------------------------------------------------------------
// spotify-controls
// ---------------------------------------------------------------------------

fn spotify_controls() -> PluginManifest {
    PluginManifest {
        name: "spotify-controls".into(),
        version: "0.1.0".into(),
        description: "Now-playing indicator in the status bar via Spotify Connect.".into(),
        author: "Phantom Team".into(),
        license: Some("MIT".into()),
        homepage: Some("https://github.com/jdmiranda/phantom/tree/main/plugins/spotify-controls".into()),
        entry_point: "spotify_controls.wasm".into(),
        permissions: vec![Permission::Network, Permission::StatusBar],
        hooks: vec![HookType::OnInterval(5)],
        commands: vec![],
        status_bar: Some(StatusBarDef {
            position: StatusBarPosition::Center,
            update_interval_ms: 5000,
        }),
    }
}

// ---------------------------------------------------------------------------
// github-notifications
// ---------------------------------------------------------------------------

fn github_notifications() -> PluginManifest {
    PluginManifest {
        name: "github-notifications".into(),
        version: "0.1.0".into(),
        description: "GitHub notification feed with status bar badge and desktop alerts.".into(),
        author: "Phantom Team".into(),
        license: Some("MIT".into()),
        homepage: Some(
            "https://github.com/jdmiranda/phantom/tree/main/plugins/github-notifications".into(),
        ),
        entry_point: "github_notifications.wasm".into(),
        permissions: vec![
            Permission::Network,
            Permission::StatusBar,
            Permission::Notifications,
        ],
        hooks: vec![HookType::OnInterval(60)],
        commands: vec![CommandDef {
            name: "gh-notif".into(),
            description: "Show GitHub notifications with filtering and mark-as-read.".into(),
            usage: "gh-notif [--unread] [--repo <owner/repo>]".into(),
        }],
        status_bar: Some(StatusBarDef {
            position: StatusBarPosition::Right,
            update_interval_ms: 60_000,
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn official_plugins_returns_all_five() {
        let plugins = official_plugins();
        assert_eq!(plugins.len(), 5);
    }

    #[test]
    fn official_plugin_names_are_unique() {
        let plugins = official_plugins();
        let mut names: Vec<&str> = plugins.iter().map(|p| p.name.as_str()).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 5);
    }

    #[test]
    fn get_official_finds_existing_plugin() {
        let plugin = get_official("git-enhanced");
        assert!(plugin.is_some());
        let plugin = plugin.unwrap();
        assert_eq!(plugin.name, "git-enhanced");
        assert_eq!(plugin.version, "0.1.0");
        assert!(plugin.permissions.contains(&Permission::ReadFiles));
        assert!(plugin.permissions.contains(&Permission::RunCommands));
    }

    #[test]
    fn get_official_returns_none_for_unknown() {
        assert!(get_official("does-not-exist").is_none());
    }

    #[test]
    fn git_enhanced_has_correct_hooks_and_commands() {
        let p = get_official("git-enhanced").unwrap();
        assert_eq!(p.hooks.len(), 1);
        assert!(matches!(&p.hooks[0], HookType::OnCommand(s) if s == "git *"));
        assert_eq!(p.commands.len(), 2);
        assert_eq!(p.commands[0].name, "git-graph");
        assert_eq!(p.commands[1].name, "git-review");
    }

    #[test]
    fn docker_dashboard_has_status_bar() {
        let p = get_official("docker-dashboard").unwrap();
        let sb = p.status_bar.as_ref().expect("docker-dashboard should have status bar");
        assert!(matches!(sb.position, StatusBarPosition::Right));
        assert_eq!(sb.update_interval_ms, 5000);
    }

    #[test]
    fn spotify_has_interval_hook_and_no_commands() {
        let p = get_official("spotify-controls").unwrap();
        assert!(matches!(&p.hooks[0], HookType::OnInterval(5)));
        assert!(p.commands.is_empty());
    }

    #[test]
    fn github_notifications_has_all_three_permissions() {
        let p = get_official("github-notifications").unwrap();
        assert!(p.permissions.contains(&Permission::Network));
        assert!(p.permissions.contains(&Permission::StatusBar));
        assert!(p.permissions.contains(&Permission::Notifications));
    }

    #[test]
    fn api_inspector_hooks_on_output() {
        let p = get_official("api-inspector").unwrap();
        assert!(matches!(&p.hooks[0], HookType::OnOutput));
        assert_eq!(p.commands.len(), 1);
        assert_eq!(p.commands[0].name, "api");
    }

    #[test]
    fn all_official_plugins_have_wasm_entry_point() {
        for p in official_plugins() {
            assert!(
                p.entry_point.ends_with(".wasm"),
                "{} entry point should be .wasm, got {}",
                p.name,
                p.entry_point
            );
        }
    }
}
