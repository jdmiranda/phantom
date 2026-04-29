//! Dispatcher — ticket-handout agent for the work queue.
//!
//! The [`GhTicketDispatcher`] is a thread-safe coordinator that queries open
//! GitHub issues via the `gh` CLI, walks the `Blocked by:` dependency DAG to
//! find the highest-priority unblocked issue matching a requester's capability
//! set, and hands it out as a [`Ticket`].
//!
//! ## Issue discovery
//!
//! [`GhTicketDispatcher::fetch_open_issues`] shells out to
//! `gh issue list --state open --json number,title,body,labels` and parses
//! the result. Each issue body is scanned for `Blocked by: #N` lines to build
//! the dependency edge set.
//!
//! ## Topological ordering
//!
//! Tickets are eligible only when none of their declared blockers appears in
//! the open-issues set. Among eligible tickets the one with the lowest issue
//! number is returned first (proxy for age / merge order). Callers supply
//! a `&[CapabilityClass]` filter; only tickets whose `scope` label matches one
//! of the requester's capabilities are considered.
//!
//! ## In-progress / done tracking
//!
//! [`GhTicketDispatcher::mark_in_progress`] — adds a `"in-progress"` label to
//! the issue via `gh issue edit --add-label` and records the claimer in an
//! in-memory [`ClaimedSet`] so a second concurrent caller cannot receive the
//! same ticket.
//!
//! [`GhTicketDispatcher::mark_done`] — closes the issue via
//! `gh issue close` with a comment linking the finishing PR URL.
//!
//! ## Thread safety
//!
//! All mutable state lives behind `Arc<Mutex<ClaimedSet>>`. Two agents calling
//! [`DispatcherTool::RequestNextTicket`] concurrently will each receive a
//! distinct ticket or `None` — the claim is recorded inside the same lock
//! acquisition that tests eligibility, so there is no TOCTOU window.
//!
//! ## DispatcherTool catalog
//!
//! [`DispatcherTool`] enumerates the three tool names the Dispatcher role
//! exposes. [`DispatcherToolContext`] bundles the shared dispatcher handle
//! plus per-turn args; the free functions [`request_next_ticket`],
//! [`mark_ticket_in_progress`], and [`mark_ticket_done`] are the handlers.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::role::CapabilityClass;

// ---------------------------------------------------------------------------
// Ticket
// ---------------------------------------------------------------------------

/// A GitHub issue handed out by the [`GhTicketDispatcher`].
///
/// Fields are read-only once constructed — all mutation goes through the
/// dispatcher methods that call the `gh` CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    /// GitHub issue number (e.g. 24).
    number: u64,
    /// Issue title.
    title: String,
    /// Scope labels from the issue (e.g. `"phantom-agents"`, `"phase-2"`).
    scope_labels: Vec<String>,
    /// Issue numbers that must be resolved before this ticket is eligible.
    blockers: Vec<u64>,
}

impl Ticket {
    /// GitHub issue number.
    #[must_use]
    pub fn number(&self) -> u64 {
        self.number
    }

    /// Issue title.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Scope labels.
    #[must_use]
    pub fn scope_labels(&self) -> &[String] {
        &self.scope_labels
    }

    /// Blocker issue numbers.
    #[must_use]
    pub fn blockers(&self) -> &[u64] {
        &self.blockers
    }
}

// ---------------------------------------------------------------------------
// ClaimedSet — tracks in-progress tickets
// ---------------------------------------------------------------------------

/// Tracks which issue numbers are currently claimed (in-progress) by an agent.
///
/// All access is through [`GhTicketDispatcher`] which holds the `Arc<Mutex<…>>`
/// so concurrent callers are serialised at the claim boundary.
#[derive(Debug, Default)]
struct ClaimedSet {
    /// issue_number → claimer label
    claimed: HashMap<u64, String>,
}

impl ClaimedSet {
    fn new() -> Self {
        Self { claimed: HashMap::new() }
    }

    fn is_claimed(&self, number: u64) -> bool {
        self.claimed.contains_key(&number)
    }

    fn claim(&mut self, number: u64, claimer: impl Into<String>) {
        self.claimed.insert(number, claimer.into());
    }

    fn release(&mut self, number: u64) {
        self.claimed.remove(&number);
    }
}

// ---------------------------------------------------------------------------
// GhIssue — intermediate parse shape from `gh issue list --json`
// ---------------------------------------------------------------------------

/// Raw JSON shape returned by `gh issue list --json number,title,body,labels`.
#[derive(Debug, Deserialize)]
struct GhIssue {
    number: u64,
    title: String,
    body: Option<String>,
    labels: Vec<GhLabel>,
}

#[derive(Debug, Deserialize)]
struct GhLabel {
    name: String,
}

// ---------------------------------------------------------------------------
// Parser: extract `Blocked by: #N` lines from an issue body
// ---------------------------------------------------------------------------

/// Parse `Blocked by: #N, #M, …` lines from an issue body.
///
/// Matches lines of the form (case-insensitive):
/// ```text
/// Blocked by: #24
/// Blocked by: #24, #30
/// **Blocked by**: #24
/// ```
///
/// Returns the set of blocker issue numbers found. An empty set means the
/// ticket is unblocked (or has no declared blockers).
fn parse_blockers(body: &str) -> Vec<u64> {
    let mut out = Vec::new();
    for line in body.lines() {
        let lower = line.to_lowercase();
        // Strip markdown bold markers so `**Blocked by**:` also matches.
        let stripped = lower
            .replace("**blocked by**", "blocked by")
            .replace("**blocked by:**", "blocked by:");
        if let Some(rest) = stripped.strip_prefix("blocked by:") {
            // rest = " #24, #30" etc.
            for tok in rest.split([',', ' ', '\t']) {
                if let Some(num_str) = tok.strip_prefix('#') {
                    if let Ok(n) = num_str.parse::<u64>() {
                        out.push(n);
                    }
                }
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

// ---------------------------------------------------------------------------
// GhTicketDispatcher
// ---------------------------------------------------------------------------

/// Trait for fetching open issues — separates the `gh` CLI from tests.
///
/// The production implementation calls `gh`; tests inject a mock.
pub trait IssueSource: Send + Sync {
    /// Return all currently open issues from the repository.
    fn fetch_open_issues(&self, repo: &str) -> Result<Vec<Ticket>, String>;
}

/// Production [`IssueSource`] that shells out to the `gh` CLI.
pub struct GhIssueSource;

impl IssueSource for GhIssueSource {
    fn fetch_open_issues(&self, repo: &str) -> Result<Vec<Ticket>, String> {
        let output = Command::new("gh")
            .args([
                "issue",
                "list",
                "--repo",
                repo,
                "--state",
                "open",
                "--json",
                "number,title,body,labels",
                "--limit",
                "200",
            ])
            .output()
            .map_err(|e| format!("gh command failed: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("gh issue list error: {stderr}"));
        }

        let raw: Vec<GhIssue> = serde_json::from_slice(&output.stdout)
            .map_err(|e| format!("gh JSON parse error: {e}"))?;

        Ok(raw.into_iter().map(gh_issue_to_ticket).collect())
    }
}

fn gh_issue_to_ticket(gh: GhIssue) -> Ticket {
    let blockers = gh
        .body
        .as_deref()
        .map(parse_blockers)
        .unwrap_or_default();
    let scope_labels = gh.labels.into_iter().map(|l| l.name).collect();
    Ticket {
        number: gh.number,
        title: gh.title,
        scope_labels,
        blockers,
    }
}

/// Thread-safe GitHub-backed ticket dispatcher.
///
/// See the [module documentation](self) for the full contract.
pub struct GhTicketDispatcher {
    repo: String,
    source: Box<dyn IssueSource>,
    claimed: Arc<Mutex<ClaimedSet>>,
}

impl GhTicketDispatcher {
    /// Create a dispatcher backed by the real `gh` CLI.
    #[must_use]
    pub fn new(repo: impl Into<String>) -> Self {
        Self {
            repo: repo.into(),
            source: Box::new(GhIssueSource),
            claimed: Arc::new(Mutex::new(ClaimedSet::new())),
        }
    }

    /// Create a dispatcher with a custom [`IssueSource`] (for testing).
    #[must_use]
    pub fn with_source(repo: impl Into<String>, source: impl IssueSource + 'static) -> Self {
        Self {
            repo: repo.into(),
            source: Box::new(source),
            claimed: Arc::new(Mutex::new(ClaimedSet::new())),
        }
    }

    /// Wrap `self` in an `Arc` for cheap cross-thread sharing.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Find and atomically claim the highest-priority unblocked ticket that
    /// matches the requester's capability set.
    ///
    /// "Highest priority" is defined as lowest issue number (oldest first).
    ///
    /// A ticket is eligible iff:
    /// 1. None of its `blockers` appears in the current open-issues set.
    /// 2. It is not already claimed (in-flight) by another requester.
    /// 3. At least one of its `scope_labels` is matched by `capabilities` —
    ///    OR the label set is empty (no scope restriction declared).
    ///
    /// The claim is recorded atomically inside the lock, so two concurrent
    /// callers cannot receive the same ticket.
    ///
    /// Returns `Ok(Some(ticket))` on success, `Ok(None)` when no eligible
    /// ticket exists, and `Err(_)` on fetch failures.
    pub fn request_next_ticket(
        &self,
        requester_role: &str,
        capabilities: &[CapabilityClass],
    ) -> Result<Option<Ticket>, String> {
        let issues = self.source.fetch_open_issues(&self.repo)?;
        let open_numbers: HashSet<u64> = issues.iter().map(|t| t.number).collect();

        let mut guard = self
            .claimed
            .lock()
            .map_err(|_| "dispatcher claimed-set mutex poisoned".to_string())?;

        let _ = requester_role; // carried for logging / future prompt injection

        // Walk issues in ascending number order (oldest = highest priority).
        let mut sorted: Vec<&Ticket> = issues.iter().collect();
        sorted.sort_by_key(|t| t.number);

        for ticket in sorted {
            // Already claimed by another agent → skip.
            if guard.is_claimed(ticket.number) {
                continue;
            }

            // Has an open blocker → skip.
            let blocked = ticket
                .blockers
                .iter()
                .any(|b| open_numbers.contains(b));
            if blocked {
                continue;
            }

            // Scope filter: if the ticket declares any scope labels, at least
            // one must overlap the requester's capabilities. An issue with no
            // scope labels is universally eligible.
            if !ticket.scope_labels.is_empty() {
                let matches = ticket.scope_labels.iter().any(|label| {
                    capability_matches_label(capabilities, label)
                });
                if !matches {
                    continue;
                }
            }

            // Atomically record the claim.
            guard.claim(ticket.number, "claimed");
            return Ok(Some(ticket.clone()));
        }

        Ok(None)
    }

    /// Add the `in-progress` label via `gh` and record the claimer.
    ///
    /// Returns `Ok("marked #N in-progress (claimer)")` on success.
    pub fn mark_in_progress(
        &self,
        issue_number: u64,
        claimer: &str,
    ) -> Result<String, String> {
        let status = Command::new("gh")
            .args([
                "issue",
                "edit",
                &issue_number.to_string(),
                "--repo",
                &self.repo,
                "--add-label",
                "in-progress",
            ])
            .status()
            .map_err(|e| format!("gh command failed: {e}"))?;

        if !status.success() {
            return Err(format!(
                "gh issue edit failed for #{issue_number} (exit {})",
                status.code().unwrap_or(-1),
            ));
        }

        let mut guard = self
            .claimed
            .lock()
            .map_err(|_| "dispatcher claimed-set mutex poisoned".to_string())?;
        guard.claim(issue_number, claimer);

        Ok(format!("marked #{issue_number} in-progress ({claimer})"))
    }

    /// Close the issue via `gh` and release the in-memory claim.
    ///
    /// Adds a comment with the `pr_url` before closing so the issue links to
    /// the finishing PR. Returns `Ok("closed #N")` on success.
    pub fn mark_done(&self, issue_number: u64, pr_url: &str) -> Result<String, String> {
        // Comment with the PR URL.
        let comment_status = Command::new("gh")
            .args([
                "issue",
                "comment",
                &issue_number.to_string(),
                "--repo",
                &self.repo,
                "--body",
                &format!("Resolved by {pr_url}"),
            ])
            .status()
            .map_err(|e| format!("gh comment failed: {e}"))?;

        if !comment_status.success() {
            // Non-fatal: continue to close even if the comment fails.
            // Tests and CI environments may lack comment permissions.
        }

        // Close the issue.
        let close_status = Command::new("gh")
            .args([
                "issue",
                "close",
                &issue_number.to_string(),
                "--repo",
                &self.repo,
            ])
            .status()
            .map_err(|e| format!("gh issue close failed: {e}"))?;

        if !close_status.success() {
            return Err(format!(
                "gh issue close failed for #{issue_number} (exit {})",
                close_status.code().unwrap_or(-1),
            ));
        }

        let mut guard = self
            .claimed
            .lock()
            .map_err(|_| "dispatcher claimed-set mutex poisoned".to_string())?;
        guard.release(issue_number);

        Ok(format!("closed #{issue_number}"))
    }
}

// ---------------------------------------------------------------------------
// Scope-label → capability matching
// ---------------------------------------------------------------------------

/// Returns `true` when at least one of `capabilities` logically matches `label`.
///
/// The mapping is intentionally loose: a label of `"phantom-agents"` or
/// `"coordination"` matches `Coordinate`; `"inspect"` or `"sense"` matches
/// `Sense`. Unrecognised labels are treated as universally eligible so that
/// issues with generic labels (e.g. `"bug"`, `"enhancement"`) are not
/// silently hidden from all requesters.
fn capability_matches_label(capabilities: &[CapabilityClass], label: &str) -> bool {
    let lower = label.to_lowercase();

    for cap in capabilities {
        let matched = match cap {
            CapabilityClass::Sense => {
                lower.contains("sense")
                    || lower.contains("inspect")
                    || lower.contains("observe")
                    || lower.contains("read")
            }
            CapabilityClass::Coordinate => {
                lower.contains("coordinat")
                    || lower.contains("dispatch")
                    || lower.contains("phantom-agents")
                    || lower.contains("orchestrat")
            }
            CapabilityClass::Act => {
                lower.contains("act")
                    || lower.contains("write")
                    || lower.contains("mutate")
                    || lower.contains("run")
            }
            CapabilityClass::Compute => {
                lower.contains("compute")
                    || lower.contains("llm")
                    || lower.contains("embed")
                    || lower.contains("ai")
            }
            CapabilityClass::Reflect => {
                lower.contains("reflect")
                    || lower.contains("memory")
                    || lower.contains("log")
            }
        };
        if matched {
            return true;
        }
    }

    // Unrecognised label → universally eligible (don't silently drop tickets).
    !capabilities.is_empty()
        && !is_known_scope_label(&lower)
}

/// Return `true` for labels that the matching logic explicitly understands.
///
/// Only labels the matcher *knows about* can reject a ticket. A label not
/// in this set is treated as universally eligible so generic GitHub labels
/// (`"bug"`, `"enhancement"`, `"Phase 2"`) never accidentally filter out
/// tickets that every requester should see.
fn is_known_scope_label(lower: &str) -> bool {
    lower.contains("sense")
        || lower.contains("inspect")
        || lower.contains("observe")
        || lower.contains("read")
        || lower.contains("coordinat")
        || lower.contains("dispatch")
        || lower.contains("phantom-agents")
        || lower.contains("orchestrat")
        || lower.contains("act")
        || lower.contains("write")
        || lower.contains("mutate")
        || lower.contains("run")
        || lower.contains("compute")
        || lower.contains("llm")
        || lower.contains("embed")
        || lower.contains("reflect")
        || lower.contains("memory")
}

// ---------------------------------------------------------------------------
// DispatcherTool catalog
// ---------------------------------------------------------------------------

/// The three tool ids exposed by the Dispatcher role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DispatcherTool {
    /// Claim the next available ticket. Capability: `Coordinate`.
    RequestNextTicket,
    /// Mark a claimed ticket as in-progress. Capability: `Coordinate`.
    MarkTicketInProgress,
    /// Mark a claimed ticket as done / close it. Capability: `Coordinate`.
    MarkTicketDone,
}

impl DispatcherTool {
    /// Wire name used in tool definitions and JSON dispatch.
    #[must_use]
    pub fn api_name(&self) -> &'static str {
        match self {
            Self::RequestNextTicket => "request_next_ticket",
            Self::MarkTicketInProgress => "mark_ticket_in_progress",
            Self::MarkTicketDone => "mark_ticket_done",
        }
    }

    /// Parse from wire name. Returns `None` for unknown names.
    #[must_use]
    pub fn from_api_name(name: &str) -> Option<Self> {
        match name {
            "request_next_ticket" => Some(Self::RequestNextTicket),
            "mark_ticket_in_progress" => Some(Self::MarkTicketInProgress),
            "mark_ticket_done" => Some(Self::MarkTicketDone),
            _ => None,
        }
    }

    /// The capability class the calling role must declare to invoke this tool.
    #[must_use]
    pub fn class(&self) -> CapabilityClass {
        // All three Dispatcher tools are Coordinate-class: they coordinate
        // which agent works on which issue.
        CapabilityClass::Coordinate
    }
}

// ---------------------------------------------------------------------------
// Tool context and handlers
// ---------------------------------------------------------------------------

/// Context handed to every [`DispatcherTool`] handler.
#[derive(Clone)]
pub struct DispatcherToolContext {
    dispatcher: Arc<GhTicketDispatcher>,
}

impl DispatcherToolContext {
    /// Construct a context wrapping a shared dispatcher.
    #[must_use]
    pub fn new(dispatcher: Arc<GhTicketDispatcher>) -> Self {
        Self { dispatcher }
    }
}

// ---- Argument decoders ----

#[derive(Debug, Deserialize)]
struct RequestNextTicketArgs {
    requester_role: String,
    #[serde(default)]
    requester_capabilities: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct MarkTicketInProgressArgs {
    issue_number: u64,
    claimer: String,
}

#[derive(Debug, Deserialize)]
struct MarkTicketDoneArgs {
    issue_number: u64,
    pr_url: String,
}

// ---- Capability string → CapabilityClass ----

fn parse_capability(s: &str) -> Option<CapabilityClass> {
    match s.to_lowercase().as_str() {
        "sense" => Some(CapabilityClass::Sense),
        "reflect" => Some(CapabilityClass::Reflect),
        "compute" => Some(CapabilityClass::Compute),
        "act" => Some(CapabilityClass::Act),
        "coordinate" => Some(CapabilityClass::Coordinate),
        _ => None,
    }
}

// ---- Handler functions ----

/// Claim the next available ticket from the GH issue tracker.
///
/// `args` must contain:
/// - `requester_role`: string (the calling agent's role label)
/// - `requester_capabilities`: array of capability strings (e.g. `["Sense","Coordinate"]`)
///
/// Returns `Ok(Some(Ticket))` when a ticket is available, `Ok(None)` when
/// the queue is empty or all remaining tickets are blocked/claimed.
pub fn request_next_ticket(
    args: &serde_json::Value,
    ctx: &DispatcherToolContext,
) -> Result<Option<Ticket>, String> {
    let parsed: RequestNextTicketArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid request_next_ticket args: {e}"))?;

    let caps: Vec<CapabilityClass> = parsed
        .requester_capabilities
        .iter()
        .filter_map(|s| parse_capability(s))
        .collect();

    ctx.dispatcher.request_next_ticket(&parsed.requester_role, &caps)
}

/// Mark an issue as in-progress.
///
/// `args` must contain:
/// - `issue_number`: u64
/// - `claimer`: string (label for the claiming agent)
pub fn mark_ticket_in_progress(
    args: &serde_json::Value,
    ctx: &DispatcherToolContext,
) -> Result<String, String> {
    let parsed: MarkTicketInProgressArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid mark_ticket_in_progress args: {e}"))?;
    ctx.dispatcher.mark_in_progress(parsed.issue_number, &parsed.claimer)
}

/// Mark an issue as done (closes it via gh).
///
/// `args` must contain:
/// - `issue_number`: u64
/// - `pr_url`: string (URL of the finishing PR)
pub fn mark_ticket_done(
    args: &serde_json::Value,
    ctx: &DispatcherToolContext,
) -> Result<String, String> {
    let parsed: MarkTicketDoneArgs = serde_json::from_value(args.clone())
        .map_err(|e| format!("invalid mark_ticket_done args: {e}"))?;
    ctx.dispatcher.mark_done(parsed.issue_number, &parsed.pr_url)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::thread;

    // ---- Helpers ----

    /// Build a [`Ticket`] with explicit fields for test fixtures.
    fn ticket(number: u64, title: &str, blockers: Vec<u64>) -> Ticket {
        Ticket {
            number,
            title: title.into(),
            scope_labels: vec![],
            blockers,
        }
    }

    fn ticket_with_labels(number: u64, title: &str, blockers: Vec<u64>, labels: Vec<&str>) -> Ticket {
        Ticket {
            number,
            title: title.into(),
            scope_labels: labels.into_iter().map(|s| s.to_string()).collect(),
            blockers,
        }
    }

    /// Mock [`IssueSource`] that returns a pre-configured ticket list.
    struct MockSource {
        issues: Arc<Mutex<Vec<Ticket>>>,
    }

    impl MockSource {
        fn new(issues: Vec<Ticket>) -> Self {
            Self { issues: Arc::new(Mutex::new(issues)) }
        }
    }

    impl IssueSource for MockSource {
        fn fetch_open_issues(&self, _repo: &str) -> Result<Vec<Ticket>, String> {
            Ok(self.issues.lock().unwrap().clone())
        }
    }

    fn dispatcher_with(issues: Vec<Ticket>) -> GhTicketDispatcher {
        GhTicketDispatcher::with_source("test/repo", MockSource::new(issues))
    }

    // ---- parse_blockers ----

    #[test]
    fn parse_blockers_extracts_single() {
        let body = "Blocked by: #24";
        assert_eq!(parse_blockers(body), vec![24]);
    }

    #[test]
    fn parse_blockers_extracts_multiple() {
        let body = "Blocked by: #24, #30";
        assert_eq!(parse_blockers(body), vec![24, 30]);
    }

    #[test]
    fn parse_blockers_handles_bold_markdown() {
        let body = "**Blocked by**: #10\n**Blocked by**: #20";
        let mut got = parse_blockers(body);
        got.sort();
        assert_eq!(got, vec![10, 20]);
    }

    #[test]
    fn parse_blockers_empty_when_not_present() {
        let body = "Just a normal issue body with no blockers.";
        assert!(parse_blockers(body).is_empty());
    }

    #[test]
    fn parse_blockers_is_case_insensitive() {
        let body = "blocked by: #5, #7";
        assert_eq!(parse_blockers(body), vec![5, 7]);
    }

    // ---- DispatcherTool catalog ----

    #[test]
    fn dispatcher_tool_api_name_round_trip() {
        for tool in [
            DispatcherTool::RequestNextTicket,
            DispatcherTool::MarkTicketInProgress,
            DispatcherTool::MarkTicketDone,
        ] {
            let parsed = DispatcherTool::from_api_name(tool.api_name());
            assert_eq!(parsed, Some(tool), "round-trip failed for {tool:?}");
        }
    }

    #[test]
    fn dispatcher_tool_unknown_returns_none() {
        assert!(DispatcherTool::from_api_name("not_a_tool").is_none());
    }

    #[test]
    fn dispatcher_tool_class_is_coordinate() {
        for tool in [
            DispatcherTool::RequestNextTicket,
            DispatcherTool::MarkTicketInProgress,
            DispatcherTool::MarkTicketDone,
        ] {
            assert_eq!(
                tool.class(),
                CapabilityClass::Coordinate,
                "{tool:?} must require Coordinate"
            );
        }
    }

    // ---- Topological ordering: 5 issues with dependency DAG ----------------
    //
    // DAG:
    //   #1 (no deps) ← unblocked
    //   #2 blocked by #1
    //   #3 blocked by #1, #2
    //   #4 (no deps) ← unblocked
    //   #5 blocked by #4
    //
    // With all 5 open, topological order is:
    //   wave 1: #1, #4   (no open blockers)
    //   wave 2: #2, #5   (once #1/#4 are gone from open set)
    //   wave 3: #3       (once #1 and #2 are gone)
    //
    // The dispatcher returns the lowest-numbered eligible ticket each call.
    // We simulate resolution by removing claimed issues from the mock source.

    #[test]
    fn topological_ordering_five_issue_dag() {
        let all_issues = vec![
            ticket(1, "Foundation work", vec![]),
            ticket(2, "Needs #1", vec![1]),
            ticket(3, "Needs #1 and #2", vec![1, 2]),
            ticket(4, "Independent track", vec![]),
            ticket(5, "Needs #4", vec![4]),
        ];

        // Simulate the full schedule by repeatedly calling request_next_ticket,
        // removing the claimed ticket from the open set each time (as if an
        // agent completed it).
        let mut open: Vec<Ticket> = all_issues.clone();
        let mut order: Vec<u64> = Vec::new();

        while !open.is_empty() {
            let d = dispatcher_with(open.clone());
            let ticket = d
                .request_next_ticket("Dispatcher", &[CapabilityClass::Coordinate])
                .expect("fetch ok")
                .expect("at least one unblocked ticket");

            order.push(ticket.number());

            // Remove the claimed ticket from the open set (simulate completion).
            open.retain(|t| t.number != ticket.number());
        }

        // Verify constraints:
        // #1 before #2 (since #2 blocked by #1)
        // #1 before #3 (since #3 blocked by #1)
        // #2 before #3 (since #3 blocked by #2)
        // #4 before #5 (since #5 blocked by #4)
        let pos = |n: u64| order.iter().position(|&x| x == n).unwrap();
        assert!(pos(1) < pos(2), "wrong order: #1 must precede #2");
        assert!(pos(1) < pos(3), "wrong order: #1 must precede #3");
        assert!(pos(2) < pos(3), "wrong order: #2 must precede #3");
        assert!(pos(4) < pos(5), "wrong order: #4 must precede #5");
        assert_eq!(order.len(), 5, "all 5 tickets must be claimed exactly once");
    }

    // ---- First wave: lowest-numbered unblocked ticket returned first --------

    #[test]
    fn returns_lowest_numbered_unblocked_ticket() {
        let d = dispatcher_with(vec![
            ticket(10, "mid priority", vec![]),
            ticket(3, "highest priority (lowest number)", vec![]),
            ticket(20, "lowest priority", vec![]),
        ]);
        let t = d
            .request_next_ticket("Dispatcher", &[CapabilityClass::Coordinate])
            .unwrap()
            .unwrap();
        assert_eq!(t.number(), 3);
    }

    // ---- Blocked ticket is never returned ----------------------------------

    #[test]
    fn blocked_ticket_is_not_returned() {
        let d = dispatcher_with(vec![
            ticket(1, "blocker", vec![]),
            ticket(2, "blocked by #1", vec![1]),
        ]);

        // #1 is open so #2 must not be returned.
        let t = d
            .request_next_ticket("Dispatcher", &[CapabilityClass::Coordinate])
            .unwrap()
            .unwrap();
        assert_eq!(t.number(), 1, "#1 (unblocked) must be returned before #2");
    }

    #[test]
    fn all_blocked_returns_none() {
        // #1 and #2 are both open; #2 is blocked by #1; #1 is blocked by #2.
        // Circular dependency — neither is eligible.
        let d = dispatcher_with(vec![
            ticket(1, "blocked by #2", vec![2]),
            ticket(2, "blocked by #1", vec![1]),
        ]);
        let result = d
            .request_next_ticket("Dispatcher", &[CapabilityClass::Coordinate])
            .unwrap();
        assert!(result.is_none(), "circularly-blocked tickets must not be returned");
    }

    // ---- Concurrent requests — no duplicate hand-outs ----------------------
    //
    // Acceptance test: two threads concurrently calling request_next_ticket
    // with N tickets in the queue must each receive a distinct ticket.
    // The total claimed count must equal N.

    #[test]
    fn concurrent_requests_get_distinct_tickets() {
        const N: usize = 32;

        let issues: Vec<Ticket> = (1..=(N as u64))
            .map(|n| ticket(n, &format!("task-{n}"), vec![]))
            .collect();

        let d = Arc::new(GhTicketDispatcher::with_source(
            "test/repo",
            MockSource::new(issues),
        ));

        const THREADS: usize = 8;
        let handles: Vec<_> = (0..THREADS)
            .map(|_| {
                let d = Arc::clone(&d);
                thread::spawn(move || {
                    let mut claimed = Vec::new();
                    // Each thread grabs tickets until the queue is empty.
                    loop {
                        match d.request_next_ticket("Dispatcher", &[CapabilityClass::Coordinate]) {
                            Ok(Some(t)) => claimed.push(t.number()),
                            Ok(None) => break,
                            Err(e) => panic!("fetch error: {e}"),
                        }
                    }
                    claimed
                })
            })
            .collect();

        let mut all: Vec<u64> = handles
            .into_iter()
            .flat_map(|h| h.join().expect("thread panicked"))
            .collect();
        all.sort();

        assert_eq!(all.len(), N, "all {N} tickets must be claimed exactly once");
        let unique: HashSet<u64> = all.iter().copied().collect();
        assert_eq!(unique.len(), N, "each ticket must be claimed by exactly one thread");
    }

    // ---- Scope-label capability filtering ----------------------------------

    #[test]
    fn ticket_with_no_scope_labels_is_eligible_for_any_requester() {
        let d = dispatcher_with(vec![ticket(1, "no labels", vec![])]);
        let t = d
            .request_next_ticket("anyone", &[CapabilityClass::Sense])
            .unwrap();
        assert!(t.is_some(), "ticket with no scope labels must be universally eligible");
    }

    #[test]
    fn ticket_with_matching_scope_label_is_returned() {
        let d = dispatcher_with(vec![
            ticket_with_labels(1, "dispatch work", vec![], vec!["phantom-agents"]),
        ]);
        let t = d
            .request_next_ticket("Dispatcher", &[CapabilityClass::Coordinate])
            .unwrap();
        assert!(t.is_some(), "ticket matching Coordinate scope must be returned");
    }

    // ---- Ticket accessor round-trips ----------------------------------------

    #[test]
    fn ticket_accessors_are_correct() {
        let t = Ticket {
            number: 42,
            title: "hello".into(),
            scope_labels: vec!["bug".into()],
            blockers: vec![1, 2],
        };
        assert_eq!(t.number(), 42);
        assert_eq!(t.title(), "hello");
        assert_eq!(t.scope_labels(), &["bug".to_string()]);
        assert_eq!(t.blockers(), &[1u64, 2u64]);
    }
}
