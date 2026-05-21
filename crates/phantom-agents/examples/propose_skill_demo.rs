//! End-to-end demonstration of the `propose_skill` self-extension primitive.
//!
//! Run:
//! ```bash
//! cargo run -p phantom-agents --example propose_skill_demo
//! ```
//!
//! Creates a staging directory under a temporary path, makes a handful of
//! proposals (one Composer-authored, one brain-provenance-attached), shows
//! the resulting markdown and audit-log lines, then demonstrates the
//! sanitization and capability-gate rejections. The output is the proof:
//! the artifacts exist on disk and the rejections are returned as the
//! tool would surface them to the model.

use std::path::Path;

use phantom_agents::role::{AgentRef, AgentRole, CapabilityClass, SpawnSource};
use phantom_agents::self_extension_tools::{propose_skill, SelfExtensionTool};
use serde_json::json;

fn rule(label: &str) {
    println!();
    println!("==== {label} {}", "=".repeat(72_usize.saturating_sub(label.len() + 5)));
}

fn show_file(path: &Path) {
    println!("\n--- {} ---", path.display());
    match std::fs::read_to_string(path) {
        Ok(s) => print!("{s}"),
        Err(e) => println!("(read error: {e})"),
    }
}

fn show_audit(staging_dir: &Path) {
    let path = staging_dir.join("audit.log");
    println!("\n--- {} ---", path.display());
    match std::fs::read_to_string(&path) {
        Ok(s) => {
            for (i, line) in s.lines().enumerate() {
                if line.is_empty() {
                    continue;
                }
                let parsed: serde_json::Value = serde_json::from_str(line).expect("audit JSON");
                println!(
                    "[{}] {}",
                    i,
                    serde_json::to_string_pretty(&parsed).unwrap_or_default()
                );
            }
        }
        Err(e) => println!("(read error: {e})"),
    }
}

fn main() {
    // Staging area: a clean temp dir per run so output is reproducible.
    let tmp = tempfile::tempdir().expect("tempdir");
    let project_dir = tmp.path();
    let staging = project_dir.join(".phantom").join("proposed-skills");
    println!("Project root: {}", project_dir.display());
    println!("Staging dir : {}", staging.display());

    // -----------------------------------------------------------------------
    rule("1. Tool catalogue (the model-facing surface)");
    println!("api_name:        {}", SelfExtensionTool::ProposeSkill.api_name());
    println!(
        "round-trip:      from_api_name({:?}) = {:?}",
        SelfExtensionTool::ProposeSkill.api_name(),
        SelfExtensionTool::from_api_name(SelfExtensionTool::ProposeSkill.api_name()),
    );
    println!(
        "capability:      {:?}",
        SelfExtensionTool::ProposeSkill.class(),
    );

    // -----------------------------------------------------------------------
    rule("2. Capability gate (default-deny by role manifest)");
    let roles = [
        AgentRole::Composer,
        AgentRole::Conversational,
        AgentRole::Actor,
        AgentRole::Watcher,
        AgentRole::Reflector,
        AgentRole::Fixer,
        AgentRole::Defender,
        AgentRole::Dispatcher,
        AgentRole::Cartographer,
        AgentRole::Capturer,
    ];
    let want = SelfExtensionTool::ProposeSkill.class();
    println!("Required class: {want:?}");
    println!();
    println!("{:<16}  {}", "ROLE", "CAN PROPOSE?");
    for role in roles {
        let granted = role.has(CapabilityClass::Reflect);
        println!(
            "{:<16}  {}",
            role.label(),
            if granted { "yes" } else { "DENIED" }
        );
    }

    // -----------------------------------------------------------------------
    rule("3. Happy path — Composer proposes a skill");
    let composer = AgentRef::new(42, AgentRole::Composer, "composer-demo", SpawnSource::User);
    let args = json!({
        "name": "rust-style-check",
        "description": "Lints Rust source for project style violations: let-else, newtypes, no comments-on-obvious",
        "body": "# rust-style-check\n\nWhen reviewing Rust changes, enforce:\n- for-loops over .iter().for_each\n- let-else for early returns\n- variable shadowing for type narrowing\n- newtypes around primitive ids\n\nFail closed: any uncertain match is reported, not silently accepted.\n",
        "rationale": "We've burned three review cycles on the same style issues; codifying the rules saves review bandwidth and frees reviewers for substantive feedback",
    });
    match propose_skill(&args, &composer, project_dir) {
        Ok(path) => {
            println!("returned path: {}", path.display());
            show_file(&path);
        }
        Err(e) => panic!("happy path failed: {e}"),
    }

    // -----------------------------------------------------------------------
    rule("4. Brain-sourced proposal — carries score + candidate provenance");
    let candidate = json!({
        "external_id": "jdmiranda/phantom#999",
        "source": "gh-issues",
        "title": "auto-discovered: same lint hits this repo three weeks in a row",
        "labels": ["enhancement", "developer-experience"],
    });
    let score = json!({
        "priority_rank": 0.30,
        "age_hours": 0.10,
        "activity_count": 0.05,
        "labels_bonus": 0.15,
        "weighted_sum": 0.78,
        "critical_floor_applied": false,
    });
    let args = json!({
        "name": "lint-recurring-fix",
        "description": "Repository skill for the recurring lint issue tracked in #999",
        "body": "# Recurring lint fix\n\nAuto-proposed by the phantom-brain self-improvement reconciler after detecting #999 cross three consecutive CI failures. Refer to `docs/design/brain-self-improvement.md` for scoring details.\n",
        "rationale": "phantom-brain score_candidate returned 0.78, above the Standard band threshold (0.75) — clear win for codifying as a skill rather than another one-off PR.",
        "source_candidate": candidate,
        "score": score,
    });
    match propose_skill(&args, &composer, project_dir) {
        Ok(path) => {
            println!("returned path: {}", path.display());
            show_file(&path);
        }
        Err(e) => panic!("brain-sourced proposal failed: {e}"),
    }

    // -----------------------------------------------------------------------
    rule("5. Unified audit log — both proposals visible to a single `tail -F`");
    show_audit(&staging);

    // -----------------------------------------------------------------------
    rule("6. Sanitization rejections (defense in depth)");
    let bad_names = [
        ("path traversal",        "../etc/passwd"),
        ("forward slash",         "foo/bar"),
        ("backslash",             "foo\\bar"),
        ("leading dot",           ".hidden"),
        ("unicode (non-ASCII)",   "naïve-skill"),
        ("emoji",                 "deploy-🚀"),
        ("empty",                 ""),
        ("whitespace only",       "    "),
        ("only separators",       "---"),
    ];
    for (label, name) in bad_names {
        let args = json!({
            "name": name,
            "description": "would never get this far",
            "body": "b",
            "rationale": "r",
        });
        let res = propose_skill(&args, &composer, project_dir);
        match res {
            Ok(_) => println!("[{:<22}] LEAKED — input {name:?} should have been rejected", label),
            Err(e) => println!("[{:<22}] rejected: {e}", label),
        }
    }

    // -----------------------------------------------------------------------
    rule("7. Oversized body rejection");
    let big_body = "x".repeat(40_001);
    let args = json!({
        "name": "too-big",
        "description": "d",
        "body": big_body,
        "rationale": "r",
    });
    match propose_skill(&args, &composer, project_dir) {
        Ok(_) => println!("LEAKED — oversized body should have been rejected"),
        Err(e) => println!("rejected: {e}"),
    }

    // -----------------------------------------------------------------------
    rule("8. Same-millisecond collision handling");
    // We can't reliably force a collision in real time, so we plant a sentinel
    // at the expected timestamped path and then propose; the function should
    // bump to a -1 suffix rather than overwriting.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let sentinel = staging.join(format!("{now}-collide-test.md"));
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(&sentinel, b"SENTINEL must not be overwritten").unwrap();
    println!("planted sentinel at: {}", sentinel.display());

    let args = json!({
        "name": "collide-test",
        "description": "d",
        "body": "fresh body",
        "rationale": "r",
    });
    match propose_skill(&args, &composer, project_dir) {
        Ok(path) => {
            println!("new proposal path : {}", path.display());
            println!(
                "sentinel preserved: {}",
                std::fs::read_to_string(&sentinel).unwrap_or_default()
                    == "SENTINEL must not be overwritten"
            );
            // The new path differs only if a collision was actually detected
            // (i.e. now equals the sentinel ms). On slower hardware the ms
            // may have already advanced — print so we see which branch.
            println!(
                "collision branch  : {}",
                if path == sentinel { "OVERWROTE (bug)" } else { "DISTINCT (safe)" },
            );
        }
        Err(e) => println!("collide-test proposal failed: {e}"),
    }

    // -----------------------------------------------------------------------
    rule("9. Staging dir listing");
    let mut entries: Vec<_> = std::fs::read_dir(&staging)
        .expect("staging readable")
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in &entries {
        let len = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        println!("  {:>6}  {}", len, p.file_name().unwrap().to_string_lossy());
    }
    println!("\n{} total entries", entries.len());

    rule("done — every artifact above is on disk under the staging dir");
}
