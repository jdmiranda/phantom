# TD Learning + Sidecar Pattern in Phantom

## TD Learning → Adaptive Utility Scoring

The brain's `UtilityScorer` uses static weights. TD learning makes them adaptive.

### Current (static)
```
fix_score(fresh_error, idle) → always 0.9
explain_score(idle_15s, has_errors) → always 0.7
watcher_score(active_process) → always 0.5
```

### With TD (adaptive)
```
Q(state, action) ← Q(state, action) + α[reward + γ·Q(s', a') - Q(state, action)]
```

**State features**: idle_time, error_count, error_type, project_type, time_of_day,
recent_command, terminal_content_hash

**Actions**: ShowSuggestion(fix), ShowSuggestion(explain), SpawnAgent, DoNothing

**Reward signals** (already observable, currently discarded):
- User selects option [f/e/y] → +1.0 (accepted)
- User presses Escape → -0.3 (dismissed)
- User ignores (suggestion expires) → -0.5 (noise)
- Agent completes successfully → +2.0 (high-value action)
- Agent fails → -1.0 (wasted compute)
- Same error recurs after fix suggestion → -0.5 (fix didn't help)

**Implementation path**:
1. Log (state, action, reward) tuples to `~/.config/phantom/td_log.jsonl`
2. On brain startup, load and compute Q-table from history
3. In `evaluate()`, use Q-values as score adjustments: `score *= (1.0 + q_adjustment)`
4. After each reward signal, update Q-table and persist

**Why TD and not full RL**: TD(0) is online, incremental, and doesn't need
episodes to terminate. The brain scores on every event — this is continuous,
not episodic. TD fits naturally.

**Risk**: Score collapse (all actions go to 0 if user dismisses everything
during a debugging session). Mitigation: floor at 0.1 * base_score, slow
learning rate (α=0.05), and separate Q-tables per project.

## Sidecar Pattern → Multi-Process Architecture

### Existing sidecars in Phantom
1. **Brain thread** — observes render loop, acts through channels
2. **Agent API threads** — one per agent, non-blocking poll
3. **Sysmon thread** — background system metrics
4. **MCP listener** — Unix socket server for external tools
5. **Supervisor process** — watchdog + control plane

### Phase 6 sidecar extensions
- **MCP tool servers** — external processes the brain federates
- **Ollama process** — local LLM as a sidecar (already health-checked)
- **File watcher** — inotify/FSEvents sidecar for FileChanged events

### Phantom as developer sidecar
The terminal itself is a sidecar to the developer's workflow:
- Observes without interrupting (ambient brain)
- Acts when confidence is high (utility scoring)
- Maintains itself (selftest/selfheal)
- Gets smarter over time (skill memory, TD learning)

## References
- Sutton & Barto, "Reinforcement Learning" Ch. 6 (TD Learning)
- Watkins, "Q-Learning" (1989) — off-policy TD
- Burns et al., "Design Patterns for Container-Based Distributed Systems" (2016) — sidecar pattern
