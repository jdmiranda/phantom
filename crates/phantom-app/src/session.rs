// Session save/restore
//
// limitation: Full session persistence (terminal buffer, agent state, goal/task
// ledger) is tracked in #76 (agent state), #77 (goal state), and the
// phantom-session crate. This file is a placeholder — actual session I/O lives
// in `crates/phantom-session/src/lib.rs`. When phantom-session is wired into
// the app lifecycle (Phase 3), this module will grow to coordinate the
// save-on-shutdown and restore-on-startup hooks. // see #76, #77
