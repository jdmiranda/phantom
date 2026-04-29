//! Self-test + self-heal system — the AI brain exercises its own house,
//! diagnoses failures, fixes its own code, and ships the fix.
//!
//! Loop: `selftest` → detect failures → build repair context → spawn
//! agent to fix → `cargo test` → commit → push. The brain maintains
//! itself.
//!
//! Triggered by:
//! - Console command `selftest` (test only, report to console)
//! - Console command `selfheal` (test → fix → verify → commit → push)

use crate::app::App;

// ---------------------------------------------------------------------------
// Test definitions
// ---------------------------------------------------------------------------

struct TestCase {
    name: &'static str,
    action: TestAction,
    check: TestCheck,
    /// Which files are involved (for repair context).
    files: &'static [&'static str],
}

#[allow(dead_code)]
enum TestAction {
    None,
    ShowSuggestion(&'static str),
    DismissSuggestion,
    DetachToFloat,
    DockToGrid,
    MoveFloat(f32, f32),
    ResizeFloat(f32, f32),
    OpenContextMenu(f32, f32),
    CloseContextMenu,
    FocusByIndex(usize),
}

#[allow(dead_code)]
enum TestCheck {
    SuggestionVisible(&'static str),
    SuggestionHidden,
    HistoryMinLen(usize),
    FocusedIsFloating,
    FocusedIsTiled,
    FloatPosition(f32, f32),
    FloatSize(f32, f32),
    ContextMenuVisible,
    ContextMenuHidden,
    HasAdapters,
    BrainAlive,
    AlwaysPass,
}

// ---------------------------------------------------------------------------
// Failure diagnosis
// ---------------------------------------------------------------------------

/// A failed test with enough context for an AI agent to fix it.
#[derive(Debug, Clone)]
pub(crate) struct Failure {
    pub name: String,
    pub expected: String,
    pub actual: String,
    pub files: Vec<String>,
}

impl Failure {
    /// Build a repair prompt an agent can act on.
    fn repair_prompt(&self) -> String {
        format!(
            "SELFTEST FAILURE: \"{}\"\n\
             Expected: {}\n\
             Actual: {}\n\
             Relevant files: {}\n\n\
             Diagnose why this self-test fails and fix the code. \
             After fixing, run `cargo test --workspace` to verify. \
             The fix should be minimal — don't refactor unrelated code.",
            self.name,
            self.expected,
            self.actual,
            self.files.join(", "),
        )
    }
}

// ---------------------------------------------------------------------------
// Runner
// ---------------------------------------------------------------------------

/// The self-test/self-heal state machine.
pub(crate) struct SelfTestRunner {
    tests: Vec<TestCase>,
    current: usize,
    passed: usize,
    failed: usize,
    failures: Vec<Failure>,
    phase: Phase,
    done: bool,
    /// If true, spawn repair agents for failures and commit fixes.
    heal_mode: bool,
    /// Tracks the heal pipeline stage after tests complete.
    heal_stage: HealStage,
}

enum Phase {
    Act,
    Check,
}

#[derive(PartialEq)]
#[allow(dead_code)]
enum HealStage {
    /// Haven't entered heal pipeline yet.
    Pending,
    /// Repair agent spawned, waiting for completion.
    Repairing,
    /// Repair done, verifying with cargo test.
    Verifying,
    /// Verification passed, committing.
    Committing,
    /// All done.
    Complete,
}

impl SelfTestRunner {
    pub fn new(heal: bool) -> Self {
        Self {
            tests: build_test_suite(),
            current: 0,
            passed: 0,
            failed: 0,
            failures: Vec::new(),
            phase: Phase::Act,
            done: false,
            heal_mode: heal,
            heal_stage: HealStage::Pending,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Advance one step. Returns console output lines.
    pub fn tick(&mut self, app: &mut App) -> Vec<String> {
        // If tests are still running, run the next test.
        if self.current < self.tests.len() {
            return self.tick_tests(app);
        }

        // Tests are done. If no failures or not in heal mode, we're done.
        if self.failures.is_empty() || !self.heal_mode {
            if !self.done {
                self.done = true;
                let mut out = vec![self.summary()];
                if !self.failures.is_empty() && !self.heal_mode {
                    out.push("Run `selfheal` to auto-fix failures.".into());
                }
                return out;
            }
            return vec![];
        }

        // Heal pipeline.
        self.tick_heal(app)
    }

    fn tick_tests(&mut self, app: &mut App) -> Vec<String> {
        let mut output = Vec::new();

        match self.phase {
            Phase::Act => {
                let test = &self.tests[self.current];
                execute_action(&test.action, app);
                self.phase = Phase::Check;
            }
            Phase::Check => {
                let test = &self.tests[self.current];
                let (result, actual_desc) = check_result_detailed(&test.check, app);

                if result {
                    self.passed += 1;
                    output.push(format!("  \x1b[32mPASS\x1b[0m  {}", test.name));
                } else {
                    self.failed += 1;
                    output.push(format!("  \x1b[31mFAIL\x1b[0m  {}", test.name));
                    self.failures.push(Failure {
                        name: test.name.to_string(),
                        expected: describe_check(&test.check),
                        actual: actual_desc,
                        files: test.files.iter().map(|s| s.to_string()).collect(),
                    });
                }

                cleanup_after_test(app);
                self.current += 1;
                self.phase = Phase::Act;

                if self.current >= self.tests.len() {
                    output.push(self.summary());
                }
            }
        }

        output
    }

    fn tick_heal(&mut self, app: &mut App) -> Vec<String> {
        let mut output = Vec::new();

        match self.heal_stage {
            HealStage::Pending => {
                let repair_prompt = self.build_combined_repair_prompt();
                output.push(format!(
                    "SELFHEAL: {} failure(s) detected. Spawning autonomous repair agent...",
                    self.failures.len()
                ));

                // Spawn an internal FixError agent with tool use. The agent
                // can read_file, edit_file, run_command, and git operations
                // — no external CLI needed. The tool-use loop in agent_pane
                // handles the execute → re-invoke → iterate cycle.
                let first_file = self.failures.first()
                    .and_then(|f| f.files.first().cloned());

                app.pending_brain_actions.push(
                    phantom_brain::events::AiAction::SpawnAgent {
                        task: phantom_agents::AgentTask::FixError {
                            error_summary: format!(
                                "{} selftest failure(s)",
                                self.failures.len()
                            ),
                            file: first_file,
                            context: repair_prompt,
                        },
                        spawn_tag: None,
                    }
                );

                self.heal_stage = HealStage::Repairing;
            }
            HealStage::Repairing => {
                // The agent is now autonomous — it will read files, edit code,
                // run tests, and commit via its tool-use loop. The agent pane
                // shows progress in real-time.
                output.push("SELFHEAL: Repair agent is autonomous. Watch its pane for progress.".into());
                output.push("SELFHEAL: The agent can read_file, edit_file, run_command, git_status.".into());
                output.push("SELFHEAL: When done, run `selftest` to verify.".into());
                self.heal_stage = HealStage::Complete;
                self.done = true;
            }
            HealStage::Verifying | HealStage::Committing => {
                // These stages are handled by Claude in the terminal now.
                self.heal_stage = HealStage::Complete;
                self.done = true;
            }
            HealStage::Complete => {}
        }

        output
    }

    fn build_combined_repair_prompt(&self) -> String {
        let mut prompt = String::from(
            "Phantom selftest detected the following failures. \
             Fix each one. The codebase is a Rust workspace with deny(warnings).\n\n"
        );
        for (i, f) in self.failures.iter().enumerate() {
            prompt.push_str(&format!("--- Failure {} ---\n{}\n\n", i + 1, f.repair_prompt()));
        }
        prompt.push_str(
            "After fixing, run `cargo test --workspace` and ensure 0 failures. \
             Make the minimal change needed."
        );
        prompt
    }

    fn summary(&self) -> String {
        let total = self.passed + self.failed;
        if self.failed == 0 {
            format!("SELFTEST: {total}/{total} passed. All systems operational.")
        } else {
            format!(
                "SELFTEST: {}/{total} passed, {} FAILED.",
                self.passed, self.failed
            )
        }
    }
}

// ---------------------------------------------------------------------------
// Action execution
// ---------------------------------------------------------------------------

fn execute_action(action: &TestAction, app: &mut App) {
    match action {
        TestAction::None => {}
        TestAction::ShowSuggestion(text) => {
            app.suggestion = Some(crate::app::SuggestionOverlay {
                text: text.to_string(),
                options: vec![],
                shown_at: std::time::Instant::now(),
            });
        }
        TestAction::DismissSuggestion => {
            if let Some(dismissed) = app.suggestion.take() {
                app.suggestion_history.push_back(dismissed);
                if app.suggestion_history.len() > 10 {
                    app.suggestion_history.pop_front();
                }
            }
        }
        TestAction::DetachToFloat => {
            if let Some(focused) = app.coordinator.focused() {
                if !app.coordinator.is_floating(focused) {
                    app.coordinator.detach_to_float(focused, &mut app.layout, &mut app.scene);
                }
            }
        }
        TestAction::DockToGrid => {
            if let Some(focused) = app.coordinator.focused() {
                if app.coordinator.is_floating(focused) {
                    app.coordinator.dock_to_grid(focused, &mut app.layout, &mut app.scene);
                }
            }
        }
        TestAction::MoveFloat(x, y) => {
            if let Some(focused) = app.coordinator.focused() {
                app.coordinator.move_floating(focused, *x, *y);
            }
        }
        TestAction::ResizeFloat(w, h) => {
            if let Some(focused) = app.coordinator.focused() {
                app.coordinator.resize_floating(focused, *w, *h);
            }
        }
        TestAction::OpenContextMenu(x, y) => {
            let items = vec![
                crate::context_menu::MenuItem {
                    label: "Test Item".into(),
                    action: crate::context_menu::MenuAction::Copy,
                    enabled: true,
                },
            ];
            app.context_menu.show(*x, *y, items);
        }
        TestAction::CloseContextMenu => {
            app.context_menu.hide();
        }
        TestAction::FocusByIndex(idx) => {
            let ids = app.coordinator.all_app_ids();
            if let Some(&id) = ids.get(*idx) {
                app.coordinator.set_focus(id);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Check execution (with diagnostic detail)
// ---------------------------------------------------------------------------

fn check_result_detailed(check: &TestCheck, app: &App) -> (bool, String) {
    match check {
        TestCheck::SuggestionVisible(text) => {
            let ok = app.suggestion.as_ref().is_some_and(|s| s.text.contains(text));
            let actual = app.suggestion.as_ref().map_or("None".into(), |s| s.text.clone());
            (ok, format!("suggestion = {actual}"))
        }
        TestCheck::SuggestionHidden => {
            let ok = app.suggestion.is_none();
            (ok, format!("suggestion present = {}", app.suggestion.is_some()))
        }
        TestCheck::HistoryMinLen(n) => {
            let len = app.suggestion_history.len();
            (len >= *n, format!("history len = {len}"))
        }
        TestCheck::FocusedIsFloating => {
            let floating = app.coordinator.focused().is_some_and(|id| app.coordinator.is_floating(id));
            (floating, format!("focused floating = {floating}"))
        }
        TestCheck::FocusedIsTiled => {
            let tiled = app.coordinator.focused().is_some_and(|id| !app.coordinator.is_floating(id));
            (tiled, format!("focused tiled = {tiled}"))
        }
        TestCheck::FloatPosition(x, y) => {
            let pos = app.coordinator.focused()
                .and_then(|id| app.coordinator.float_rect(id))
                .map(|r| format!("({:.0}, {:.0})", r.x, r.y))
                .unwrap_or("None".into());
            let ok = app.coordinator.focused()
                .and_then(|id| app.coordinator.float_rect(id))
                .is_some_and(|r| (r.x - x).abs() < 5.0 && (r.y - y).abs() < 5.0);
            (ok, format!("position = {pos}"))
        }
        TestCheck::FloatSize(w, h) => {
            let size = app.coordinator.focused()
                .and_then(|id| app.coordinator.float_rect(id))
                .map(|r| format!("({:.0}, {:.0})", r.width, r.height))
                .unwrap_or("None".into());
            let ok = app.coordinator.focused()
                .and_then(|id| app.coordinator.float_rect(id))
                .is_some_and(|r| (r.width - w).abs() < 5.0 && (r.height - h).abs() < 5.0);
            (ok, format!("size = {size}"))
        }
        TestCheck::ContextMenuVisible => {
            (app.context_menu.visible, format!("visible = {}", app.context_menu.visible))
        }
        TestCheck::ContextMenuHidden => {
            (!app.context_menu.visible, format!("visible = {}", app.context_menu.visible))
        }
        TestCheck::HasAdapters => {
            let count = app.coordinator.all_app_ids().len();
            (count > 0, format!("adapter count = {count}"))
        }
        TestCheck::BrainAlive => {
            let alive = app.brain.is_some();
            (alive, format!("brain = {}", if alive { "alive" } else { "dead" }))
        }
        TestCheck::AlwaysPass => (true, "always".into()),
    }
}

fn describe_check(check: &TestCheck) -> String {
    match check {
        TestCheck::SuggestionVisible(t) => format!("suggestion visible containing \"{t}\""),
        TestCheck::SuggestionHidden => "no suggestion visible".into(),
        TestCheck::HistoryMinLen(n) => format!("history len >= {n}"),
        TestCheck::FocusedIsFloating => "focused adapter is floating".into(),
        TestCheck::FocusedIsTiled => "focused adapter is tiled".into(),
        TestCheck::FloatPosition(x, y) => format!("float position near ({x}, {y})"),
        TestCheck::FloatSize(w, h) => format!("float size near ({w}, {h})"),
        TestCheck::ContextMenuVisible => "context menu visible".into(),
        TestCheck::ContextMenuHidden => "context menu hidden".into(),
        TestCheck::HasAdapters => "at least one adapter registered".into(),
        TestCheck::BrainAlive => "brain is alive".into(),
        TestCheck::AlwaysPass => "always pass".into(),
    }
}

fn cleanup_after_test(app: &mut App) {
    app.suggestion = None;
    app.context_menu.hide();
    let floating: Vec<_> = app.coordinator.floating_ids().collect();
    for id in floating {
        app.coordinator.dock_to_grid(id, &mut app.layout, &mut app.scene);
    }
}

// ---------------------------------------------------------------------------
// Test suite
// ---------------------------------------------------------------------------

fn build_test_suite() -> Vec<TestCase> {
    vec![
        TestCase {
            name: "adapters registered",
            action: TestAction::None,
            check: TestCheck::HasAdapters,
            files: &["crates/phantom-app/src/coordinator.rs"],
        },
        TestCase {
            name: "brain is alive",
            action: TestAction::None,
            check: TestCheck::BrainAlive,
            files: &["crates/phantom-brain/src/brain.rs", "crates/phantom-app/src/app.rs"],
        },
        TestCase {
            name: "show suggestion",
            action: TestAction::ShowSuggestion("Test suggestion"),
            check: TestCheck::SuggestionVisible("Test suggestion"),
            files: &["crates/phantom-app/src/app.rs"],
        },
        TestCase {
            name: "dismiss suggestion",
            action: TestAction::DismissSuggestion,
            check: TestCheck::SuggestionHidden,
            files: &["crates/phantom-app/src/input.rs"],
        },
        TestCase {
            name: "suggestion saved to history",
            action: TestAction::None,
            check: TestCheck::HistoryMinLen(1),
            files: &["crates/phantom-app/src/input.rs", "crates/phantom-app/src/app.rs"],
        },
        TestCase {
            name: "detach to float",
            action: TestAction::DetachToFloat,
            check: TestCheck::FocusedIsFloating,
            files: &["crates/phantom-app/src/coordinator.rs"],
        },
        TestCase {
            name: "move floating pane",
            action: TestAction::MoveFloat(200.0, 150.0),
            check: TestCheck::FloatPosition(200.0, 150.0),
            files: &["crates/phantom-app/src/coordinator.rs"],
        },
        TestCase {
            name: "resize floating pane",
            action: TestAction::ResizeFloat(500.0, 350.0),
            check: TestCheck::FloatSize(500.0, 350.0),
            files: &["crates/phantom-app/src/coordinator.rs"],
        },
        TestCase {
            name: "min size enforced",
            action: TestAction::ResizeFloat(10.0, 10.0),
            check: TestCheck::FloatSize(100.0, 80.0),
            files: &["crates/phantom-app/src/coordinator.rs"],
        },
        TestCase {
            name: "dock back to grid",
            action: TestAction::DockToGrid,
            check: TestCheck::FocusedIsTiled,
            files: &["crates/phantom-app/src/coordinator.rs"],
        },
        TestCase {
            name: "open context menu",
            action: TestAction::OpenContextMenu(100.0, 200.0),
            check: TestCheck::ContextMenuVisible,
            files: &["crates/phantom-app/src/context_menu.rs"],
        },
        TestCase {
            name: "close context menu",
            action: TestAction::CloseContextMenu,
            check: TestCheck::ContextMenuHidden,
            files: &["crates/phantom-app/src/context_menu.rs"],
        },
    ]
}
