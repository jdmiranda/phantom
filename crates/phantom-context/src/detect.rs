use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::context::ProjectCommands;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Detected project type based on marker files in the project root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProjectType {
    Rust,
    Node,
    Python,
    Go,
    Java,
    Ruby,
    Elixir,
    Cpp,
    CSharp,
    Swift,
    Unknown,
}

/// Detected package manager.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PackageManager {
    Cargo,
    Npm,
    Yarn,
    Pnpm,
    Bun,
    Pip,
    Poetry,
    Uv,
    GoMod,
    Maven,
    Gradle,
    Bundler,
    Mix,
    CMake,
    Make,
    SPM,
    Unknown,
}

/// Detected framework or tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Framework {
    // JS ecosystem
    React,
    NextJs,
    Vue,
    Svelte,
    Angular,
    // Rust ecosystem
    Actix,
    Axum,
    Rocket,
    Tauri,
    // Python ecosystem
    Django,
    Flask,
    FastAPI,
    // Ruby
    Rails,
    // Elixir
    Phoenix,
    // Java
    Spring,
    // Nothing detected
    None,
}

// ---------------------------------------------------------------------------
// Project type detection
// ---------------------------------------------------------------------------

/// Detect project type from marker files in `dir`.
pub fn detect_project(dir: &Path) -> ProjectType {
    // Order matters: check the most distinctive markers first.
    if dir.join("Cargo.toml").exists() {
        return ProjectType::Rust;
    }
    if dir.join("go.mod").exists() {
        return ProjectType::Go;
    }
    if dir.join("mix.exs").exists() {
        return ProjectType::Elixir;
    }
    if dir.join("Gemfile").exists() {
        return ProjectType::Ruby;
    }
    if dir.join("Package.swift").exists() {
        return ProjectType::Swift;
    }
    if dir.join("pom.xml").exists() || dir.join("build.gradle").exists() {
        return ProjectType::Java;
    }
    if dir.join("CMakeLists.txt").exists() {
        return ProjectType::Cpp;
    }
    if has_csharp_marker(dir) {
        return ProjectType::CSharp;
    }
    if dir.join("package.json").exists() {
        return ProjectType::Node;
    }
    if dir.join("pyproject.toml").exists()
        || dir.join("requirements.txt").exists()
        || dir.join("setup.py").exists()
    {
        return ProjectType::Python;
    }
    ProjectType::Unknown
}

/// Check for .sln or .csproj files in the directory.
fn has_csharp_marker(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if let Some(ext) = path.extension() {
            let ext = ext.to_string_lossy();
            if ext == "sln" || ext == "csproj" {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Package manager detection
// ---------------------------------------------------------------------------

/// Detect the package manager for a project directory.
pub fn detect_package_manager(dir: &Path) -> PackageManager {
    // Rust
    if dir.join("Cargo.toml").exists() {
        return PackageManager::Cargo;
    }

    // Node — disambiguate by lockfile
    if dir.join("package.json").exists() {
        if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            return PackageManager::Bun;
        }
        if dir.join("pnpm-lock.yaml").exists() {
            return PackageManager::Pnpm;
        }
        if dir.join("yarn.lock").exists() {
            return PackageManager::Yarn;
        }
        return PackageManager::Npm;
    }

    // Python
    if dir.join("pyproject.toml").exists() {
        // Peek inside to determine poetry vs uv vs pip
        if let Ok(contents) = std::fs::read_to_string(dir.join("pyproject.toml")) {
            if contents.contains("[tool.poetry]") {
                return PackageManager::Poetry;
            }
            if contents.contains("[tool.uv]") {
                return PackageManager::Uv;
            }
        }
        return PackageManager::Pip;
    }
    if dir.join("requirements.txt").exists() || dir.join("setup.py").exists() {
        return PackageManager::Pip;
    }

    // Go
    if dir.join("go.mod").exists() {
        return PackageManager::GoMod;
    }

    // Java
    if dir.join("pom.xml").exists() {
        return PackageManager::Maven;
    }
    if dir.join("build.gradle").exists() {
        return PackageManager::Gradle;
    }

    // Ruby
    if dir.join("Gemfile").exists() {
        return PackageManager::Bundler;
    }

    // Elixir
    if dir.join("mix.exs").exists() {
        return PackageManager::Mix;
    }

    // C++
    if dir.join("CMakeLists.txt").exists() {
        return PackageManager::CMake;
    }
    if dir.join("Makefile").exists() {
        return PackageManager::Make;
    }

    // Swift
    if dir.join("Package.swift").exists() {
        return PackageManager::SPM;
    }

    PackageManager::Unknown
}

// ---------------------------------------------------------------------------
// Framework detection
// ---------------------------------------------------------------------------

/// Detect the framework used by reading config/manifest file contents.
pub fn detect_framework(dir: &Path, project_type: &ProjectType) -> Framework {
    match project_type {
        ProjectType::Node => detect_node_framework(dir),
        ProjectType::Rust => detect_rust_framework(dir),
        ProjectType::Python => detect_python_framework(dir),
        ProjectType::Ruby => detect_ruby_framework(dir),
        ProjectType::Elixir => detect_elixir_framework(dir),
        ProjectType::Java => detect_java_framework(dir),
        _ => Framework::None,
    }
}

fn detect_node_framework(dir: &Path) -> Framework {
    let Ok(contents) = std::fs::read_to_string(dir.join("package.json")) else {
        return Framework::None;
    };

    // Next.js check before React — Next includes React as a dep.
    if contents.contains("\"next\"") {
        return Framework::NextJs;
    }
    if contents.contains("\"@angular/core\"") {
        return Framework::Angular;
    }
    if contents.contains("\"svelte\"") || contents.contains("\"@sveltejs/") {
        return Framework::Svelte;
    }
    if contents.contains("\"vue\"") {
        return Framework::Vue;
    }
    if contents.contains("\"react\"") {
        return Framework::React;
    }
    Framework::None
}

fn detect_rust_framework(dir: &Path) -> Framework {
    let Ok(contents) = std::fs::read_to_string(dir.join("Cargo.toml")) else {
        return Framework::None;
    };

    if contents.contains("tauri") {
        return Framework::Tauri;
    }
    if contents.contains("actix") {
        return Framework::Actix;
    }
    if contents.contains("axum") {
        return Framework::Axum;
    }
    if contents.contains("rocket") {
        return Framework::Rocket;
    }
    Framework::None
}

fn detect_python_framework(dir: &Path) -> Framework {
    // Collect text from pyproject.toml and requirements.txt
    let mut haystack = String::new();
    if let Ok(s) = std::fs::read_to_string(dir.join("pyproject.toml")) {
        haystack.push_str(&s);
    }
    if let Ok(s) = std::fs::read_to_string(dir.join("requirements.txt")) {
        haystack.push_str(&s);
    }

    if haystack.contains("django") || haystack.contains("Django") {
        return Framework::Django;
    }
    if haystack.contains("fastapi") || haystack.contains("FastAPI") {
        return Framework::FastAPI;
    }
    if haystack.contains("flask") || haystack.contains("Flask") {
        return Framework::Flask;
    }
    Framework::None
}

fn detect_ruby_framework(dir: &Path) -> Framework {
    let Ok(contents) = std::fs::read_to_string(dir.join("Gemfile")) else {
        return Framework::None;
    };
    if contents.contains("rails") {
        return Framework::Rails;
    }
    Framework::None
}

fn detect_elixir_framework(dir: &Path) -> Framework {
    let Ok(contents) = std::fs::read_to_string(dir.join("mix.exs")) else {
        return Framework::None;
    };
    if contents.contains("phoenix") {
        return Framework::Phoenix;
    }
    Framework::None
}

fn detect_java_framework(dir: &Path) -> Framework {
    let mut haystack = String::new();
    if let Ok(s) = std::fs::read_to_string(dir.join("pom.xml")) {
        haystack.push_str(&s);
    }
    if let Ok(s) = std::fs::read_to_string(dir.join("build.gradle")) {
        haystack.push_str(&s);
    }
    if haystack.contains("spring") || haystack.contains("Spring") {
        return Framework::Spring;
    }
    Framework::None
}

// ---------------------------------------------------------------------------
// Command detection
// ---------------------------------------------------------------------------

/// Detect standard build/test/run/lint/format commands for a project.
pub fn detect_commands(
    dir: &Path,
    project_type: &ProjectType,
    pm: &PackageManager,
) -> ProjectCommands {
    match project_type {
        ProjectType::Rust => ProjectCommands {
            build: Some("cargo build".into()),
            test: Some("cargo test".into()),
            run: Some("cargo run".into()),
            lint: Some("cargo clippy".into()),
            format: Some("cargo fmt".into()),
        },
        ProjectType::Node => {
            let prefix = match pm {
                PackageManager::Yarn => "yarn",
                PackageManager::Pnpm => "pnpm",
                PackageManager::Bun => "bun",
                _ => "npm run",
            };
            let mut cmds = ProjectCommands {
                build: None,
                test: None,
                run: None,
                lint: None,
                format: None,
            };
            if let Ok(contents) = std::fs::read_to_string(dir.join("package.json")) {
                if contents.contains("\"build\"") {
                    cmds.build = Some(format!("{prefix} build"));
                }
                if contents.contains("\"test\"") {
                    cmds.test = Some(format!("{prefix} test"));
                }
                if contents.contains("\"start\"") {
                    cmds.run = Some(format!("{prefix} start"));
                } else if contents.contains("\"dev\"") {
                    cmds.run = Some(format!("{prefix} dev"));
                }
                if contents.contains("\"lint\"") {
                    cmds.lint = Some(format!("{prefix} lint"));
                }
                if contents.contains("\"format\"") {
                    cmds.format = Some(format!("{prefix} format"));
                }
            }
            cmds
        }
        ProjectType::Python => {
            let run_cmd = if dir.join("manage.py").exists() {
                Some("python manage.py runserver".into())
            } else {
                None
            };
            ProjectCommands {
                build: None,
                test: Some("pytest".into()),
                run: run_cmd,
                lint: Some("ruff check .".into()),
                format: Some("ruff format .".into()),
            }
        }
        ProjectType::Go => ProjectCommands {
            build: Some("go build ./...".into()),
            test: Some("go test ./...".into()),
            run: Some("go run .".into()),
            lint: Some("golangci-lint run".into()),
            format: Some("gofmt -w .".into()),
        },
        ProjectType::Java => {
            let is_maven = matches!(pm, PackageManager::Maven);
            if is_maven {
                ProjectCommands {
                    build: Some("mvn compile".into()),
                    test: Some("mvn test".into()),
                    run: Some("mvn exec:java".into()),
                    lint: None,
                    format: None,
                }
            } else {
                ProjectCommands {
                    build: Some("gradle build".into()),
                    test: Some("gradle test".into()),
                    run: Some("gradle run".into()),
                    lint: None,
                    format: None,
                }
            }
        }
        ProjectType::Ruby => ProjectCommands {
            build: None,
            test: Some("bundle exec rspec".into()),
            run: Some("bundle exec rails server".into()),
            lint: Some("bundle exec rubocop".into()),
            format: None,
        },
        ProjectType::Elixir => ProjectCommands {
            build: Some("mix compile".into()),
            test: Some("mix test".into()),
            run: Some("mix phx.server".into()),
            lint: Some("mix credo".into()),
            format: Some("mix format".into()),
        },
        ProjectType::Cpp => ProjectCommands {
            build: Some("cmake --build build".into()),
            test: Some("ctest --test-dir build".into()),
            run: None,
            lint: None,
            format: Some("clang-format -i src/**/*.cpp".into()),
        },
        ProjectType::CSharp => ProjectCommands {
            build: Some("dotnet build".into()),
            test: Some("dotnet test".into()),
            run: Some("dotnet run".into()),
            lint: None,
            format: Some("dotnet format".into()),
        },
        ProjectType::Swift => ProjectCommands {
            build: Some("swift build".into()),
            test: Some("swift test".into()),
            run: Some("swift run".into()),
            lint: Some("swiftlint".into()),
            format: Some("swift-format -i -r Sources/".into()),
        },
        ProjectType::Unknown => ProjectCommands {
            build: None,
            test: None,
            run: None,
            lint: None,
            format: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        TempDir::new().unwrap()
    }

    // -- project type -------------------------------------------------------

    #[test]
    fn detect_rust_project() {
        let dir = tmp();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Rust);
    }

    #[test]
    fn detect_node_project() {
        let dir = tmp();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Node);
    }

    #[test]
    fn detect_python_project_pyproject() {
        let dir = tmp();
        std::fs::write(dir.path().join("pyproject.toml"), "").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Python);
    }

    #[test]
    fn detect_python_project_requirements() {
        let dir = tmp();
        std::fs::write(dir.path().join("requirements.txt"), "flask").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Python);
    }

    #[test]
    fn detect_go_project() {
        let dir = tmp();
        std::fs::write(dir.path().join("go.mod"), "module example").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Go);
    }

    #[test]
    fn detect_java_maven_project() {
        let dir = tmp();
        std::fs::write(dir.path().join("pom.xml"), "<project/>").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Java);
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Maven);
    }

    #[test]
    fn detect_java_gradle_project() {
        let dir = tmp();
        std::fs::write(dir.path().join("build.gradle"), "apply plugin").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Java);
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Gradle);
    }

    #[test]
    fn detect_ruby_project() {
        let dir = tmp();
        std::fs::write(dir.path().join("Gemfile"), "source 'https://rubygems.org'").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Ruby);
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Bundler);
    }

    #[test]
    fn detect_elixir_project() {
        let dir = tmp();
        std::fs::write(dir.path().join("mix.exs"), "defmodule MyApp").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Elixir);
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Mix);
    }

    #[test]
    fn detect_cpp_project() {
        let dir = tmp();
        std::fs::write(dir.path().join("CMakeLists.txt"), "cmake_minimum_required").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Cpp);
        assert_eq!(detect_package_manager(dir.path()), PackageManager::CMake);
    }

    #[test]
    fn detect_csharp_project() {
        let dir = tmp();
        std::fs::write(dir.path().join("MyApp.sln"), "").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::CSharp);
    }

    #[test]
    fn detect_swift_project() {
        let dir = tmp();
        std::fs::write(dir.path().join("Package.swift"), "// swift-tools-version").unwrap();
        assert_eq!(detect_project(dir.path()), ProjectType::Swift);
        assert_eq!(detect_package_manager(dir.path()), PackageManager::SPM);
    }

    #[test]
    fn detect_unknown_project() {
        let dir = tmp();
        assert_eq!(detect_project(dir.path()), ProjectType::Unknown);
    }

    // -- package manager ----------------------------------------------------

    #[test]
    fn detect_pnpm() {
        let dir = tmp();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Pnpm);
    }

    #[test]
    fn detect_yarn() {
        let dir = tmp();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("yarn.lock"), "").unwrap();
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Yarn);
    }

    #[test]
    fn detect_bun() {
        let dir = tmp();
        std::fs::write(dir.path().join("package.json"), "{}").unwrap();
        std::fs::write(dir.path().join("bun.lockb"), "").unwrap();
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Bun);
    }

    #[test]
    fn detect_poetry() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.poetry]\nname = \"foo\"",
        )
        .unwrap();
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Poetry);
    }

    #[test]
    fn detect_uv() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.uv]\ndev-dependencies = []",
        )
        .unwrap();
        assert_eq!(detect_package_manager(dir.path()), PackageManager::Uv);
    }

    // -- framework ----------------------------------------------------------

    #[test]
    fn detect_nextjs_framework() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"next":"14","react":"18"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_framework(dir.path(), &ProjectType::Node),
            Framework::NextJs
        );
    }

    #[test]
    fn detect_react_framework() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"dependencies":{"react":"18","react-dom":"18"}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_framework(dir.path(), &ProjectType::Node),
            Framework::React
        );
    }

    #[test]
    fn detect_axum_framework() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[dependencies]\naxum = \"0.7\"",
        )
        .unwrap();
        assert_eq!(
            detect_framework(dir.path(), &ProjectType::Rust),
            Framework::Axum
        );
    }

    #[test]
    fn detect_django_framework() {
        let dir = tmp();
        std::fs::write(dir.path().join("requirements.txt"), "django==4.2\n").unwrap();
        assert_eq!(
            detect_framework(dir.path(), &ProjectType::Python),
            Framework::Django
        );
    }

    #[test]
    fn detect_rails_framework() {
        let dir = tmp();
        std::fs::write(dir.path().join("Gemfile"), "gem 'rails', '~> 7.0'").unwrap();
        assert_eq!(
            detect_framework(dir.path(), &ProjectType::Ruby),
            Framework::Rails
        );
    }

    #[test]
    fn detect_phoenix_framework() {
        let dir = tmp();
        std::fs::write(dir.path().join("mix.exs"), "{:phoenix, \"~> 1.7\"}").unwrap();
        assert_eq!(
            detect_framework(dir.path(), &ProjectType::Elixir),
            Framework::Phoenix
        );
    }

    #[test]
    fn detect_spring_framework() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("pom.xml"),
            "<dependency>spring-boot</dependency>",
        )
        .unwrap();
        assert_eq!(
            detect_framework(dir.path(), &ProjectType::Java),
            Framework::Spring
        );
    }

    // -- commands -----------------------------------------------------------

    #[test]
    fn rust_commands() {
        let dir = tmp();
        let cmds = detect_commands(dir.path(), &ProjectType::Rust, &PackageManager::Cargo);
        assert_eq!(cmds.build.as_deref(), Some("cargo build"));
        assert_eq!(cmds.test.as_deref(), Some("cargo test"));
        assert_eq!(cmds.lint.as_deref(), Some("cargo clippy"));
    }

    #[test]
    fn node_pnpm_commands() {
        let dir = tmp();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"scripts":{"build":"vite build","test":"vitest","dev":"vite"}}"#,
        )
        .unwrap();
        let cmds = detect_commands(dir.path(), &ProjectType::Node, &PackageManager::Pnpm);
        assert_eq!(cmds.build.as_deref(), Some("pnpm build"));
        assert_eq!(cmds.test.as_deref(), Some("pnpm test"));
        assert_eq!(cmds.run.as_deref(), Some("pnpm dev"));
    }

    #[test]
    fn go_commands() {
        let dir = tmp();
        let cmds = detect_commands(dir.path(), &ProjectType::Go, &PackageManager::GoMod);
        assert_eq!(cmds.build.as_deref(), Some("go build ./..."));
        assert_eq!(cmds.format.as_deref(), Some("gofmt -w ."));
    }
}
