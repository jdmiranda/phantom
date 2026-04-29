//! Agent orchestration and pool management.
//!
//! The [`AgentManager`] owns all active agents and controls concurrency.
//! It is the single entry point for spawning, querying, and cleaning up agents.

use std::time::Duration;

use crate::agent::{Agent, AgentId, AgentStatus, AgentTask};
use crate::peer_grants::PeerGrantRegistry;

// ---------------------------------------------------------------------------
// AgentManager
// ---------------------------------------------------------------------------

/// Manages the pool of active agents.
pub struct AgentManager {
    agents: Vec<Agent>,
    next_id: AgentId,
    max_concurrent: usize,
    peer_grants: PeerGrantRegistry,
}

impl AgentManager {
    /// Create a new manager with the given concurrency limit.
    pub fn new(max_concurrent: usize) -> Self {
        Self {
            agents: Vec::new(),
            next_id: 1,
            max_concurrent,
            peer_grants: PeerGrantRegistry::new(),
        }
    }

    /// Spawn a new agent with the given task. Returns the agent ID.
    pub fn spawn(&mut self, task: AgentTask) -> AgentId {
        let id = self.next_id;
        self.next_id += 1;

        let mut agent = Agent::new(id, task);

        // If we have capacity, immediately start working.
        if self.active_count() < self.max_concurrent {
            agent.set_status(AgentStatus::Working);
        }

        self.agents.push(agent);
        id
    }

    /// Get an agent by ID (immutable).
    pub fn get(&self, id: AgentId) -> Option<&Agent> {
        self.agents.iter().find(|a| a.id() == id)
    }

    /// Get an agent by ID (mutable).
    pub fn get_mut(&mut self, id: AgentId) -> Option<&mut Agent> {
        self.agents.iter_mut().find(|a| a.id() == id)
    }

    /// Get all agents.
    pub fn agents(&self) -> &[Agent] {
        &self.agents
    }

    /// Get agents with a specific status.
    pub fn by_status(&self, status: AgentStatus) -> Vec<&Agent> {
        self.agents.iter().filter(|a| a.status() == status).collect()
    }

    /// Remove completed/failed agents older than `max_age`.
    pub fn cleanup(&mut self, max_age: Duration) {
        self.agents.retain(|agent| {
            let dominated = matches!(agent.status(), AgentStatus::Done | AgentStatus::Failed | AgentStatus::Flatline);
            if !dominated {
                return true; // keep active agents
            }
            agent.elapsed() < max_age
        });

        // Promote queued agents if capacity freed up.
        self.promote_queued();
    }

    /// How many agents are currently working (Working or WaitingForTool).
    pub fn active_count(&self) -> usize {
        self.agents
            .iter()
            .filter(|a| {
                matches!(
                    a.status(),
                    AgentStatus::Working | AgentStatus::WaitingForTool
                )
            })
            .count()
    }

    /// Is there capacity for another agent?
    pub fn has_capacity(&self) -> bool {
        self.active_count() < self.max_concurrent
    }

    /// Kill (force-complete) an agent by ID. Returns `true` if the agent existed and was killed.
    ///
    /// Killable states include all non-terminal states: `Queued`, `Planning`,
    /// `AwaitingApproval`, `Working`, and `WaitingForTool`.
    pub fn kill(&mut self, id: AgentId) -> bool {
        if let Some(agent) = self.agents.iter_mut().find(|a| a.id() == id) {
            if !agent.status().is_terminal() {
                agent.complete(false);
                agent.log("[killed by user]");
                self.promote_queued();
                return true;
            }
        }
        false
    }

    /// Kill all non-terminal agents. Returns the number of agents killed.
    ///
    /// Killable states include `Queued`, `Planning`, `AwaitingApproval`,
    /// `Working`, and `WaitingForTool`.
    pub fn kill_all(&mut self) -> usize {
        let mut count = 0;
        for agent in &mut self.agents {
            if !agent.status().is_terminal() {
                agent.complete(false);
                agent.log("[killed by user]");
                count += 1;
            }
        }
        count
    }

    /// Promote queued agents to working if capacity is available.
    fn promote_queued(&mut self) {
        let mut active = self.active_count();
        for agent in &mut self.agents {
            if active >= self.max_concurrent {
                break;
            }
            if agent.status() == AgentStatus::Queued {
                agent.set_status(AgentStatus::Working);
                active += 1;
            }
        }
    }

    /// Get the peer grant registry (immutable).
    pub fn grants(&self) -> &PeerGrantRegistry {
        &self.peer_grants
    }

    /// Get the peer grant registry (mutable).
    pub fn grants_mut(&mut self) -> &mut PeerGrantRegistry {
        &mut self.peer_grants
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn free_task(prompt: &str) -> AgentTask {
        AgentTask::FreeForm {
            prompt: prompt.into(),
        }
    }

    #[test]
    fn spawn_assigns_sequential_ids() {
        let mut mgr = AgentManager::new(4);
        let id1 = mgr.spawn(free_task("a"));
        let id2 = mgr.spawn(free_task("b"));
        let id3 = mgr.spawn(free_task("c"));
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);
    }

    #[test]
    fn spawn_starts_working_when_capacity() {
        let mut mgr = AgentManager::new(2);
        let id = mgr.spawn(free_task("a"));
        assert_eq!(mgr.get(id).unwrap().status(), AgentStatus::Working);
    }

    #[test]
    fn spawn_queues_when_at_capacity() {
        let mut mgr = AgentManager::new(1);
        let _id1 = mgr.spawn(free_task("a"));
        let id2 = mgr.spawn(free_task("b"));
        assert_eq!(mgr.get(id2).unwrap().status(), AgentStatus::Queued);
    }

    #[test]
    fn get_returns_agent() {
        let mut mgr = AgentManager::new(4);
        let id = mgr.spawn(free_task("test"));
        assert!(mgr.get(id).is_some());
        assert!(mgr.get(999).is_none());
    }

    #[test]
    fn get_mut_allows_modification() {
        let mut mgr = AgentManager::new(4);
        let id = mgr.spawn(free_task("test"));
        mgr.get_mut(id).unwrap().log("hello");
        assert_eq!(mgr.get(id).unwrap().output_log().len(), 1);
    }

    #[test]
    fn agents_returns_all() {
        let mut mgr = AgentManager::new(4);
        mgr.spawn(free_task("a"));
        mgr.spawn(free_task("b"));
        assert_eq!(mgr.agents().len(), 2);
    }

    #[test]
    fn by_status_filters() {
        let mut mgr = AgentManager::new(4);
        let id1 = mgr.spawn(free_task("a"));
        let _id2 = mgr.spawn(free_task("b"));

        mgr.get_mut(id1).unwrap().complete(true);

        assert_eq!(mgr.by_status(AgentStatus::Done).len(), 1);
        assert_eq!(mgr.by_status(AgentStatus::Working).len(), 1);
    }

    #[test]
    fn active_count_tracks_working_agents() {
        let mut mgr = AgentManager::new(4);
        let id1 = mgr.spawn(free_task("a"));
        let _id2 = mgr.spawn(free_task("b"));
        assert_eq!(mgr.active_count(), 2);

        mgr.get_mut(id1).unwrap().set_status(AgentStatus::WaitingForTool);
        assert_eq!(mgr.active_count(), 2); // WaitingForTool counts as active

        mgr.get_mut(id1).unwrap().complete(true);
        assert_eq!(mgr.active_count(), 1);
    }

    #[test]
    fn has_capacity_respects_limit() {
        let mut mgr = AgentManager::new(1);
        assert!(mgr.has_capacity());

        mgr.spawn(free_task("a"));
        assert!(!mgr.has_capacity());
    }

    #[test]
    fn cleanup_removes_old_completed_agents() {
        let mut mgr = AgentManager::new(4);
        let id = mgr.spawn(free_task("a"));
        mgr.get_mut(id).unwrap().complete(true);

        // With zero max_age, everything completed gets cleaned up.
        mgr.cleanup(Duration::ZERO);
        assert_eq!(mgr.agents().len(), 0);
    }

    #[test]
    fn cleanup_keeps_active_agents() {
        let mut mgr = AgentManager::new(4);
        let id1 = mgr.spawn(free_task("a"));
        let _id2 = mgr.spawn(free_task("b"));

        mgr.get_mut(id1).unwrap().complete(true);

        mgr.cleanup(Duration::ZERO);
        assert_eq!(mgr.agents().len(), 1); // only active one remains
    }

    #[test]
    fn cleanup_promotes_queued_to_working() {
        let mut mgr = AgentManager::new(1);
        let id1 = mgr.spawn(free_task("a")); // Working
        let id2 = mgr.spawn(free_task("b")); // Queued

        assert_eq!(mgr.get(id2).unwrap().status(), AgentStatus::Queued);

        // Complete first agent and clean up.
        mgr.get_mut(id1).unwrap().complete(true);
        mgr.cleanup(Duration::ZERO);

        // Agent 2 should now be promoted.
        assert_eq!(mgr.get(id2).unwrap().status(), AgentStatus::Working);
    }

    // -- FSM #34: kill() covers Planning + AwaitingApproval ------------------

    #[test]
    fn kill_planning_agent_sets_failed() {
        // Use capacity=0 so the agent starts as Queued, then begin_planning().
        let mut mgr = AgentManager::new(0);
        let id = mgr.spawn(free_task("plan me")); // stays Queued (no capacity)
        assert!(mgr.get_mut(id).unwrap().begin_planning(), "begin_planning must succeed from Queued");
        assert_eq!(mgr.get(id).unwrap().status(), AgentStatus::Planning);

        let killed = mgr.kill(id);
        assert!(killed, "kill() must succeed for Planning agent");
        assert_eq!(mgr.get(id).unwrap().status(), AgentStatus::Failed);
    }

    #[test]
    fn kill_awaiting_approval_agent_sets_failed() {
        let mut mgr = AgentManager::new(0); // no capacity → stays Queued
        let id = mgr.spawn(free_task("approve me"));
        {
            let a = mgr.get_mut(id).unwrap();
            assert!(a.begin_planning());
            assert!(a.submit_plan_for_approval());
        }
        assert_eq!(mgr.get(id).unwrap().status(), AgentStatus::AwaitingApproval);

        let killed = mgr.kill(id);
        assert!(killed, "kill() must succeed for AwaitingApproval agent");
        assert_eq!(mgr.get(id).unwrap().status(), AgentStatus::Failed);
    }

    #[test]
    fn kill_all_includes_planning_and_awaiting_approval() {
        // Use capacity=0 so spawned agents start as Queued and can be
        // transitioned to Planning / AwaitingApproval via the FSM helpers.
        let mut mgr = AgentManager::new(0);
        let id1 = mgr.spawn(free_task("a")); // Queued → Working (forced below)
        let id2 = mgr.spawn(free_task("b")); // Queued → Planning
        let id3 = mgr.spawn(free_task("c")); // Queued → Planning → AwaitingApproval
        let id4 = mgr.spawn(free_task("d")); // Queued → Done (terminal, not killed)

        // Force id1 into Working directly for coverage of the original kill_all path.
        mgr.get_mut(id1).unwrap().set_status(AgentStatus::Working);

        assert!(mgr.get_mut(id2).unwrap().begin_planning());
        {
            let a = mgr.get_mut(id3).unwrap();
            assert!(a.begin_planning());
            assert!(a.submit_plan_for_approval());
        }
        // id4: Queued → Done via complete (skip Working — complete() is unconditional)
        mgr.get_mut(id4).unwrap().complete(true);

        let killed = mgr.kill_all();
        // id1 (Working), id2 (Planning), id3 (AwaitingApproval) → 3 killed
        // id4 is Done (terminal) → not killed
        assert_eq!(killed, 3, "kill_all() must kill Working, Planning, and AwaitingApproval agents");
        assert_eq!(mgr.get(id4).unwrap().status(), AgentStatus::Done, "terminal agent must not be killed");
    }

    #[test]
    fn kill_done_agent_is_no_op() {
        let mut mgr = AgentManager::new(4);
        let id = mgr.spawn(free_task("a"));
        mgr.get_mut(id).unwrap().complete(true);

        let killed = mgr.kill(id);
        assert!(!killed, "kill() must not affect a Done agent");
        assert_eq!(mgr.get(id).unwrap().status(), AgentStatus::Done);
    }
}
