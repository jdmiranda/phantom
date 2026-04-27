//! Self-test system — the AI brain exercises its own house.
//!
//! Phantom's brain can introspect the app state, inject synthetic events,
//! and verify that features work correctly. Triggered by the `selftest`
//! console command or `AiAction::SelfTest`.
//!
//! The self-test runner is a state machine that advances one step per frame.
//! Each step performs an action (inject input, change state) and the next
//! step verifies the expected outcome. Results are reported to the console.

use crate::app::App;

/// A single test case the brain can run against itself.
struct TestCase {
    name: &'static str,
    action: TestAction,
    check: TestCheck,
}

/// What to do in the "act" phase.
#[allow(dead_code)]
enum TestAction {
    /// No action — just check the current state.
    None,
    /// Show a suggestion with given text.
    ShowSuggestion(&'static str),
    /// Dismiss the current suggestion (simulate Escape).
    DismissSuggestion,
    /// Detach the focused pane to float.
    DetachToFloat,
    /// Dock the focused floating pane back.
    DockToGrid,
    /// Move the floating pane to a position.
    MoveFloat(f32, f32),
    /// Resize the floating pane.
    ResizeFloat(f32, f32),
    /// Open the context menu at a position.
    OpenContextMenu(f32, f32),
    /// Close the context menu.
    CloseContextMenu,
    /// Set focus to a specific adapter by index (0 = first registered).
    FocusByIndex(usize),
}

/// What to verify after the action.
#[allow(dead_code)]
enum TestCheck {
    /// Suggestion overlay is visible with expected text.
    SuggestionVisible(&'static str),
    /// Suggestion overlay is not visible.
    SuggestionHidden,
    /// Suggestion history has at least N entries.
    HistoryMinLen(usize),
    /// Focused adapter is floating.
    FocusedIsFloating,
    /// Focused adapter is tiled (not floating).
    FocusedIsTiled,
    /// Floating pane rect is approximately at (x, y).
    FloatPosition(f32, f32),
    /// Floating pane rect is approximately (w, h).
    FloatSize(f32, f32),
    /// Context menu is visible.
    ContextMenuVisible,
    /// Context menu is hidden.
    ContextMenuHidden,
    /// Any adapter is registered (count > 0).
    HasAdapters,
    /// Brain is running.
    BrainAlive,
    /// Always passes — used for actions that just need to execute.
    AlwaysPass,
}

/// State machine that runs one step per frame.
pub(crate) struct SelfTestRunner {
    tests: Vec<TestCase>,
    current: usize,
    passed: usize,
    failed: usize,
    phase: Phase,
    done: bool,
}

enum Phase {
    Act,
    Check,
}

impl SelfTestRunner {
    pub fn new() -> Self {
        Self {
            tests: build_test_suite(),
            current: 0,
            passed: 0,
            failed: 0,
            phase: Phase::Act,
            done: false,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Advance one step. Call once per frame while the self-test is running.
    /// Returns console output lines to display.
    pub fn tick(&mut self, app: &mut App) -> Vec<String> {
        if self.done || self.current >= self.tests.len() {
            if !self.done {
                self.done = true;
                return vec![self.summary()];
            }
            return vec![];
        }

        let mut output = Vec::new();

        match self.phase {
            Phase::Act => {
                let test = &self.tests[self.current];
                execute_action(&test.action, app);
                self.phase = Phase::Check;
            }
            Phase::Check => {
                let test = &self.tests[self.current];
                let result = check_result(&test.check, app);

                if result {
                    self.passed += 1;
                    output.push(format!("  PASS  {}", test.name));
                } else {
                    self.failed += 1;
                    output.push(format!("  FAIL  {}", test.name));
                }

                // Clean up state for next test.
                cleanup_after_test(app);

                self.current += 1;
                self.phase = Phase::Act;

                if self.current >= self.tests.len() {
                    self.done = true;
                    output.push(self.summary());
                }
            }
        }

        output
    }

    fn summary(&self) -> String {
        let total = self.passed + self.failed;
        if self.failed == 0 {
            format!("SELFTEST: {total}/{total} passed. All systems operational.")
        } else {
            format!("SELFTEST: {}/{total} passed, {} FAILED.", self.passed, self.failed)
        }
    }
}

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

fn check_result(check: &TestCheck, app: &App) -> bool {
    match check {
        TestCheck::SuggestionVisible(text) => {
            app.suggestion.as_ref().is_some_and(|s| s.text.contains(text))
        }
        TestCheck::SuggestionHidden => app.suggestion.is_none(),
        TestCheck::HistoryMinLen(n) => app.suggestion_history.len() >= *n,
        TestCheck::FocusedIsFloating => {
            app.coordinator.focused().is_some_and(|id| app.coordinator.is_floating(id))
        }
        TestCheck::FocusedIsTiled => {
            app.coordinator.focused().is_some_and(|id| !app.coordinator.is_floating(id))
        }
        TestCheck::FloatPosition(x, y) => {
            app.coordinator.focused()
                .and_then(|id| app.coordinator.float_rect(id))
                .is_some_and(|r| (r.x - x).abs() < 5.0 && (r.y - y).abs() < 5.0)
        }
        TestCheck::FloatSize(w, h) => {
            app.coordinator.focused()
                .and_then(|id| app.coordinator.float_rect(id))
                .is_some_and(|r| (r.width - w).abs() < 5.0 && (r.height - h).abs() < 5.0)
        }
        TestCheck::ContextMenuVisible => app.context_menu.visible,
        TestCheck::ContextMenuHidden => !app.context_menu.visible,
        TestCheck::HasAdapters => !app.coordinator.all_app_ids().is_empty(),
        TestCheck::BrainAlive => app.brain.is_some(),
        TestCheck::AlwaysPass => true,
    }
}

fn cleanup_after_test(app: &mut App) {
    // Dismiss any lingering suggestion or menu.
    app.suggestion = None;
    app.context_menu.hide();

    // Dock any floating panes back.
    let floating: Vec<_> = app.coordinator.floating_ids().collect();
    for id in floating {
        app.coordinator.dock_to_grid(id, &mut app.layout, &mut app.scene);
    }
}

/// The built-in test suite — the brain's checklist for its own house.
fn build_test_suite() -> Vec<TestCase> {
    vec![
        // --- Foundations ---
        TestCase {
            name: "adapters registered",
            action: TestAction::None,
            check: TestCheck::HasAdapters,
        },
        TestCase {
            name: "brain is alive",
            action: TestAction::None,
            check: TestCheck::BrainAlive,
        },

        // --- Suggestion lifecycle ---
        TestCase {
            name: "show suggestion",
            action: TestAction::ShowSuggestion("Test suggestion"),
            check: TestCheck::SuggestionVisible("Test suggestion"),
        },
        TestCase {
            name: "dismiss suggestion",
            action: TestAction::DismissSuggestion,
            check: TestCheck::SuggestionHidden,
        },
        TestCase {
            name: "suggestion saved to history",
            action: TestAction::None,
            check: TestCheck::HistoryMinLen(1),
        },

        // --- Floating panes ---
        TestCase {
            name: "detach to float",
            action: TestAction::DetachToFloat,
            check: TestCheck::FocusedIsFloating,
        },
        TestCase {
            name: "move floating pane",
            action: TestAction::MoveFloat(200.0, 150.0),
            check: TestCheck::FloatPosition(200.0, 150.0),
        },
        TestCase {
            name: "resize floating pane",
            action: TestAction::ResizeFloat(500.0, 350.0),
            check: TestCheck::FloatSize(500.0, 350.0),
        },
        TestCase {
            name: "min size enforced",
            action: TestAction::ResizeFloat(10.0, 10.0),
            check: TestCheck::FloatSize(100.0, 80.0),
        },
        TestCase {
            name: "dock back to grid",
            action: TestAction::DockToGrid,
            check: TestCheck::FocusedIsTiled,
        },

        // --- Context menu ---
        TestCase {
            name: "open context menu",
            action: TestAction::OpenContextMenu(100.0, 200.0),
            check: TestCheck::ContextMenuVisible,
        },
        TestCase {
            name: "close context menu",
            action: TestAction::CloseContextMenu,
            check: TestCheck::ContextMenuHidden,
        },
    ]
}
