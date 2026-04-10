# PHANTOM

```
                                                                          
 ██████╗ ██╗  ██╗ █████╗ ███╗   ██╗████████╗ ██████╗ ███╗   ███╗
 ██╔══██╗██║  ██║██╔══██╗████╗  ██║╚══██╔══╝██╔═══██╗████╗ ████║
 ██████╔╝███████║███████║██╔██╗ ██║   ██║   ██║   ██║██╔████╔██║
 ██╔═══╝ ██╔══██║██╔══██║██║╚██╗██║   ██║   ██║   ██║██║╚██╔╝██║
 ██║     ██║  ██║██║  ██║██║ ╚████║   ██║   ╚██████╔╝██║ ╚═╝ ██║
 ╚═╝     ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝   ╚═╝    ╚═════╝ ╚═╝     ╚═╝
                                                                          
          THE TERMINAL IS DEAD. LONG LIVE THE TERMINAL.
```

---

## THE PROBLEM

Every terminal emulator in 2026 is a **dumb glass window**.

WezTerm? Beautiful glass. Ghostty? Faster glass. Kitty? Glass with pictures.
Alacritty? The fastest glass that ever glassed.

**They are all the same thing**: a rectangle that renders text, exactly like
a VT100 from 1978. Forty-eight years of "innovation" gave us... faster font rendering.

The "AI terminals" aren't better:
- **Warp**: Bolted a chatbot onto a terminal. The AI is a SIDEBAR. A tourist.
- **Wave**: Tried inline widgets. Got clunky. Abandoned by most users.
- **Fig** (Amazon Q): Autocomplete on steroids. Sold to AWS. RIP.

None of them asked the real question:

> **What if the terminal wasn't a window you type into,
> but a living system that thinks alongside you?**

---

## THE THESIS

```
 TRADITIONAL TERMINAL          PHANTOM
 +------------------------+    +----------------------------------+
 |                        |    |                                  |
 |   You type.            |    |   You work.                     |
 |   It renders.          |    |   It watches, learns, acts.     |
 |   That's it.           |    |   Agents live here.             |
 |                        |    |   It remembers everything.      |
 |   A dumb pipe.         |    |   It renders anything.          |
 |                        |    |   It knows your world.          |
 |                        |    |                                  |
 |   -------------------- |    |   It's not a terminal.          |
 |   $ echo "hello"       |    |   It's a cognitive interface.   |
 |   hello                |    |                                  |
 |   $                    |    |   You're not typing commands.   |
 |                        |    |   You're thinking out loud.     |
 +------------------------+    +----------------------------------+
```

Phantom is an **AI-native terminal emulator**. Intelligence isn't a feature.
It's the substrate. Every pixel, every keystroke, every output flows through
an awareness layer that understands what's happening and can act on it.

---

## THE ARCHITECTURE

```
 +=====================================================================+
 |                         PHANTOM ENGINE                               |
 +=====================================================================+
 |                                                                      |
 |  +------------------+  +------------------+  +------------------+   |
 |  |   RENDER ENGINE  |  |   AGENT ENGINE   |  |  CONTEXT ENGINE  |   |
 |  |                  |  |                  |  |                  |   |
 |  | GPU-accelerated  |  | Spawn, monitor,  |  | Project aware    |   |
 |  | Mixed-mode:      |  | orchestrate AI   |  | Git aware        |   |
 |  |  - terminal text |  | agents that run  |  | Language aware   |   |
 |  |  - rich widgets  |  | in sandboxed     |  | History aware    |   |
 |  |  - inline images |  | shell sessions   |  | Pattern aware    |   |
 |  |  - shader FX     |  |                  |  | Team aware       |   |
 |  +--------+---------+  +--------+---------+  +--------+---------+   |
 |           |                     |                      |            |
 |  +--------+---------------------+----------------------+---------+  |
 |  |                     SEMANTIC LAYER                            |  |
 |  |                                                               |  |
 |  |  Every command input and output is PARSED and UNDERSTOOD.     |  |
 |  |  Not as text. As MEANING.                                     |  |
 |  |                                                               |  |
 |  |  "cargo build" isn't a string. It's a BUILD ACTION on a      |  |
 |  |  RUST PROJECT that SUCCEEDED with 3 WARNINGS.                |  |
 |  |                                                               |  |
 |  |  "git push" isn't text. It's a DEPLOY EVENT to BRANCH main   |  |
 |  |  on REMOTE origin affecting 4 FILES.                          |  |
 |  +--------------------------------------------------------------+  |
 |  |                     MEMORY LAYER                              |  |
 |  |                                                               |  |
 |  |  Persistent. Per-project. Cross-session.                      |  |
 |  |  "Last time in this repo you were debugging the auth bug."    |  |
 |  |  "This project uses pnpm, not npm."                           |  |
 |  |  "Port 3001, not 3000."                                       |  |
 |  |  The terminal gets SMARTER every day you use it.              |  |
 |  +--------------------------------------------------------------+  |
 |  |                     TERMINAL EMULATOR                         |  |
 |  |                                                               |  |
 |  |  Forked from WezTerm (MIT license)                            |  |
 |  |  - Full VT100/xterm/ECMA-48 compatibility                    |  |
 |  |  - GPU rendering (OpenGL / Metal / WebGPU)                    |  |
 |  |  - Multiplexing (tabs, panes, sessions)                       |  |
 |  |  - Lua scripting                                              |  |
 |  |  - GLSL shader pipeline                                       |  |
 |  |  - Sixel + Kitty image protocol                               |  |
 |  |                                                               |  |
 |  |  We don't rewrite terminal emulation. That's solved.          |  |
 |  |  We build the BRAIN on top of proven BONES.                   |  |
 |  +--------------------------------------------------------------+  |
 +=====================================================================+
```

---

## THE SEVEN PILLARS

### PILLAR 1: THE PHANTOM AGENTS

This is the core. This is what nobody else has.

Agents aren't a chat sidebar. They're **first-class inhabitants** of your terminal.
They have their own panes. Their own shell sessions. You can WATCH them work.

```
 +=====================================================================+
 | PHANTOM v0.1                                                [X]    |
 +=====================================================================+
 |                                          |  PHANTOM AGENT #1       |
 |                                          |  Task: Fix build error  |
 |                                          |  Status: WORKING...     |
 |  YOUR TERMINAL                           |                         |
 |                                          |  > Reading src/main.rs  |
 |  $ cargo build                           |  > Found: line 42,      |
 |  error[E0308]: mismatched types          |    expected &str,        |
 |    --> src/main.rs:42:9                  |    got String            |
 |                                          |  > Reading context...    |
 |  [PHANTOM]: Build failed. 1 error.       |  > Applying fix:         |
 |  [PHANTOM]: I see the issue. Fix it?     |    .as_str() on line 42 |
 |             [Y] Apply fix                |  > Running cargo build   |
 |             [N] Explain only             |  > ................      |
 |             [A] Let agent handle it      |  > BUILD SUCCEEDED       |
 |                                          |                         |
 |  >>> Y                                   |  DONE in 4.2s           |
 |                                          |  Applied 1 fix          |
 |  [PHANTOM]: Fixed. Build passing.        |  0 errors, 0 warnings   |
 |                                          |                         |
 +------------------------------------------+-------------------------+
 |  PHANTOM AGENT #2 (background)           |  AGENT QUEUE            |
 |  Task: Run test suite                    |  #1 Fix build   DONE    |
 |  Progress: ████████████░░░ 78%           |  #2 Run tests   78%     |
 |  Passed: 142  Failed: 0  Pending: 38    |  #3 PR review   QUEUED  |
 +------------------------------------------+-------------------------+
```

**Agent capabilities:**
- Read and modify files (sandboxed to project)
- Run shell commands
- Access git
- Search the web for docs/solutions
- Talk to APIs (CI/CD, GitHub, Jira, etc.)
- Communicate with OTHER agents
- Report results as rich output (diffs, charts, tables)

**Agent spawning:**
```
 $ phantom agent "fix the failing tests in auth module"
 $ phantom agent "review my last 3 commits for issues"  
 $ phantom agent "set up a Docker compose for this project"
 $ phantom agent "watch the CI pipeline and tell me when it's green"
 $ phantom agent "refactor this function" --file src/lib.rs --line 42
```

**Or it happens automatically:**
- Build fails? Phantom highlights the error, offers to spawn an agent.
- Tests fail? Agent auto-analyzes the failure.
- You type a command wrong? Phantom suggests the right one.
- You're in a new project? Agent scans the repo and gives you a briefing.

**Agent orchestration:**
```
 $ phantom agents

 +------------------------------------------------------------------+
 |  PHANTOM AGENTS                                                   |
 +------------------------------------------------------------------+
 |  ID   | STATUS   | TASK                        | TIME   | PANE   |
 |-------|----------|-----------------------------|--------|--------|
 |  #1   | DONE     | Fix build error             | 4.2s   | R1     |
 |  #2   | RUNNING  | Run test suite              | 1m23s  | R2     |
 |  #3   | QUEUED   | Review PR #47               | --     | --     |
 |  #4   | WATCHING | Monitor CI pipeline         | 23m    | BG     |
 |  #5   | IDLE     | Standby                     | --     | --     |
 +------------------------------------------------------------------+
 |  [k]ill  [p]ause  [r]esume  [v]iew  [s]pawn new                  |
 +------------------------------------------------------------------+
```

### PILLAR 2: THE SEMANTIC LAYER

Your terminal UNDERSTANDS what's happening. Every command and output is parsed
into structured, meaningful data.

```
 WHAT YOU SEE:                    WHAT PHANTOM SEES:
 
 $ git status                     {
 On branch feature/auth             command: "git status",
 Changes not staged:                 type: "git.status",
   modified: src/auth.rs             branch: "feature/auth",
   modified: tests/auth_test.rs      upstream: "origin/feature/auth",
 Untracked files:                    modified: ["src/auth.rs",
   src/middleware.rs                             "tests/auth_test.rs"],
                                     untracked: ["src/middleware.rs"],
                                     staged: [],
                                     context: {
                                       project: "badass-cli",
                                       language: "rust",
                                       last_commit: "2h ago",
                                       ci_status: "passing"
                                     }
                                   }
```

**Why this matters:**
- Phantom can REACT to output intelligently, not just display it
- Agents understand what happened without re-running commands  
- Rich rendering knows HOW to display each type of output
- Errors are automatically categorized and solutions pre-loaded
- Your terminal history becomes a SEARCHABLE KNOWLEDGE BASE

```
 $ phantom search "that cargo error from yesterday"
 
 Found: 2025-04-08 14:23:07
 Command: cargo build
 Error: E0308 mismatched types at src/main.rs:42
 Resolution: Applied .as_str() conversion
 Agent: Phantom #1 auto-fixed in 4.2s
```

### PILLAR 3: RICH MIXED-MODE RENDERING

The terminal renders text. Phantom renders **everything**.

```
 +=====================================================================+
 |                                                                      |
 |  $ phantom render README.md                                          |
 |                                                                      |
 |  ┌─────────────────────────────────────────────────────────────┐    |
 |  │  # My Project                                               │    |
 |  │                                                             │    |
 |  │  A **really cool** project that does things.                │    |
 |  │                                                             │    |
 |  │  ## Installation                                            │    |
 |  │                                                             │    |
 |  │  ┌──────────────────────────────────────┐                   │    |
 |  │  │  $ cargo install my-project          │                   │    |
 |  │  └──────────────────────────────────────┘                   │    |
 |  └─────────────────────────────────────────────────────────────┘    |
 |                                                                      |
 |  $ git log --graph                                                   |
 |                                                                      |
 |  ┌─────────────────────────────────────────────────────────────┐    |
 |  │         INTERACTIVE COMMIT GRAPH                            │    |
 |  │                                                             │    |
 |  │  * ─── a1b2c3d feat: add auth (YOU, 2h ago)                │    |
 |  │  │                                                          │    |
 |  │  * ─── d4e5f6g fix: resolve race condition (Maria, 5h)     │    |
 |  │  │\                                                         │    |
 |  │  │ * ─ h7i8j9k refactor: clean up middleware (Alex, 1d)    │    |
 |  │  │/                                                         │    |
 |  │  * ─── k0l1m2n initial commit (YOU, 3d)                    │    |
 |  │                                                             │    |
 |  │  [Enter] inspect  [c] cherry-pick  [r] revert  [d] diff    │    |
 |  └─────────────────────────────────────────────────────────────┘    |
 |                                                                      |
 |  $ curl -s https://api.example.com/users | phantom render           |
 |                                                                      |
 |  ┌─────────────────────────────────────────────────────────────┐    |
 |  │  GET /users  200 OK  143ms                                  │    |
 |  │                                                             │    |
 |  │  ┌─ id ──┬─ name ────────┬─ role ──────┬─ active ─┐       │    |
 |  │  │  1    │ Alice Chen     │ admin       │    *     │       │    |
 |  │  │  2    │ Bob Smith      │ developer   │    *     │       │    |
 |  │  │  3    │ Carol Wu       │ designer    │          │       │    |
 |  │  └───────┴────────────────┴─────────────┴──────────┘       │    |
 |  │                                                             │    |
 |  │  3 records  [f] filter  [s] sort  [e] export                │    |
 |  └─────────────────────────────────────────────────────────────┘    |
 |                                                                      |
 +=====================================================================+
```

**What gets rich rendering:**
- JSON/YAML/TOML -> formatted, syntax-highlighted, foldable
- Markdown -> rendered with formatting
- Images (png, jpg, svg) -> displayed inline via GPU
- CSV/TSV -> interactive tables
- Git output -> interactive graphs, diffs with syntax highlighting
- Error output -> highlighted with links to source and suggested fixes
- HTTP responses -> formatted with status, headers, body
- Docker output -> live-updating container dashboards
- Test output -> progress bars, pass/fail with expandable details

**The key insight**: You don't need special commands. Phantom auto-detects
output format and upgrades it. Plain `curl` output becomes a rich API response.
Plain `git log` becomes an interactive graph. It's AUTOMATIC.

You can always press `Esc` to see raw text. Or `Tab` to toggle between
raw and rich mode. Rich mode is the default because it's BETTER.

### PILLAR 4: THE CONTEXT ENGINE

Phantom knows your world.

```
 +=====================================================================+
 | CONTEXT: badass-cli                                                  |
 +=====================================================================+
 |                                                                      |
 |  PROJECT                          ENVIRONMENT                       |
 |  Name: badass-cli                 OS: macOS 15.4                    |
 |  Type: Rust (Cargo.toml)          Shell: zsh 5.9                    |
 |  Root: ~/Documents/GitHub/        Node: v21.1.0                     |
 |        badass-cli                 Rust: 1.78.0                      |
 |                                   Docker: running (3 containers)    |
 |  GIT                                                                |
 |  Branch: feature/agents           SERVICES                          |
 |  Upstream: origin (GitHub)        postgres: UP (port 5432)          |
 |  Status: 3 modified               redis: UP (port 6379)            |
 |  Last commit: 23 min ago          dev-server: UP (port 3000)       |
 |  CI: passing (2 min ago)                                            |
 |                                   TEAM (from git)                   |
 |  RECENT COMMANDS                  You: 12 commits this week         |
 |  1. cargo build (FAIL)            Maria: 8 commits                  |
 |  2. phantom agent "fix it"        Alex: 3 commits                   |
 |  3. cargo build (OK)                                                |
 |  4. cargo test (3 failing)        MEMORY                            |
 |                                   "Auth module is being refactored" |
 |                                   "Don't touch legacy/ directory"   |
 |                                   "PR reviews needed by Friday"     |
 +=====================================================================+
```

**How context powers everything:**
- You type `build` -> Phantom knows this is a Cargo project, runs `cargo build`
- You type `test auth` -> Phantom runs `cargo test auth` (not `npm test auth`)
- An agent needs to fix a bug -> It already knows the language, framework, conventions
- You switch to a different project -> Context switches automatically
- You open a new pane -> It inherits the context of the project directory

### PILLAR 5: THE VISUAL ENGINE

Forked from WezTerm's GPU pipeline. Extended with:

```
 SHADER PIPELINE:
 
 Terminal Text Buffer
        |
        v
 +--[SHADER STAGE 1: Base Rendering]--+
 |  Font rendering (GPU-accelerated)   |
 |  Ligatures, Nerd Font icons         |
 |  True color (16M colors)            |
 +------------------------------------+
        |
        v
 +--[SHADER STAGE 2: Post-Processing]--+
 |  CRT scanlines                       |
 |  Phosphor glow / bloom               |
 |  Chromatic aberration                 |
 |  Barrel distortion (screen curve)    |
 |  Vignette                            |
 |  Film grain / noise                  |
 +-------------------------------------+
        |
        v
 +--[SHADER STAGE 3: Dynamic Effects]--+
 |  Error flash (screen flickers red    |
 |    when build fails)                 |
 |  Success pulse (green glow when      |
 |    tests pass)                       |
 |  Activity heat (panes glow brighter  |
 |    with more activity)              |
 |  Idle fade (unused panes dim)        |
 |  Agent aura (agent panes have        |
 |    subtle animated border)          |
 +-------------------------------------+
        |
        v
 +--[SHADER STAGE 4: Ambient]----------+
 |  Matrix rain (background, very faint |
 |    behind text, only in empty space) |
 |  Particle effects (on command exec)  |
 |  Background: animated or static      |
 |  Time-of-day color temperature shift |
 +-------------------------------------+
        |
        v
 Final composite -> Display

 
 REACTIVE SHADERS (the terminal FEELS alive):

 BUILD FAIL:
 +------------------------------------------+
 |                                          |
 |  $ cargo build                           |
 |  error[E0308]: mismatched types          |
 |                                          |
 |  >>> Screen briefly flashes dark red <<< |
 |  >>> CRT flicker intensifies <<<         |
 |  >>> Error lines glow brighter <<<       |
 |                                          |
 +------------------------------------------+

 BUILD SUCCESS:
 +------------------------------------------+
 |                                          |
 |  $ cargo build                           |
 |  Compiling badass-cli v0.1.0             |
 |  Finished release [optimized]            |
 |                                          |
 |  >>> Subtle green pulse across screen << |
 |  >>> Scanlines briefly brighten <<<      |
 |                                          |
 +------------------------------------------+

 AGENT WORKING:
 +------------------------------------------+
 |  +-- Agent Pane --+                      |
 |  | > Reading...   |  << Subtle animated  |
 |  | > Analyzing... |     cyan border that  |
 |  | > Fixing...    |     pulses while the  |
 |  +----------------+     agent is active   |
 +------------------------------------------+
```

### PILLAR 6: NATURAL LANGUAGE INTERFACE

You can type commands. Or just... talk.

```
 $ what changed today
 
 [PHANTOM]: 4 commits today across 2 branches:
 
   feature/agents:
     a1b2c3d  feat: add agent spawning          (you, 2h ago)
     d4e5f6g  feat: add agent pane rendering     (you, 4h ago)
   
   main:
     h7i8j9k  fix: memory leak in renderer       (Maria, 6h ago)
     k0l1m2n  docs: update API reference          (Alex, 8h ago)


 $ deploy staging

 [PHANTOM]: I'll deploy the current branch to staging.
   Branch: feature/agents
   Target: staging (Heroku)
   Commit: a1b2c3d feat: add agent spawning
   
   Proceed? [Y/n] 


 $ is CI green

 [PHANTOM]: Yes. All checks passing.
   - Build: OK (1m23s)
   - Tests: 847/847 passing (3m41s) 
   - Lint: OK (22s)
   - Security: 0 vulnerabilities
   Last run: 12 minutes ago on commit a1b2c3d


 $ show me memory usage over the last hour

 [PHANTOM]:
   Memory Usage (1h)
   4.2G ┤                              ╭──╮
   3.8G ┤                         ╭────╯  │
   3.4G ┤           ╭─────────────╯       ╰───
   3.0G ┤     ╭─────╯
   2.6G ┤─────╯
        └──────────────────────────────────────
        -60m   -45m   -30m   -15m    now
   
   Current: 3.6G / 16G (22%)
   Peak: 4.1G at -8m (cargo build)


 $ something feels slow

 [PHANTOM]: Analyzing...
   
   - CPU: 34% (normal)
   - Memory: 3.6G (normal) 
   - Disk I/O: HIGH - 89% utilization
     ^ Docker container 'postgres' is doing heavy writes
       Query: VACUUM ANALYZE on 'users' table
       ETA: ~2 minutes
   
   That's likely what you're feeling. Want me to monitor
   and notify you when it's done?
```

**The natural language layer is NOT a chatbot.** It's a COMMAND INTERPRETER.
It has access to your full context and can execute real actions. The
difference between typing `git log --oneline -5` and `show me the last 5
commits` is NOTHING. Both produce the same result.

### PILLAR 7: THE PLUGIN SYSTEM (WASM)

Phantom plugins are WebAssembly modules. Write in ANY language.

```
 phantom-plugins/
 +-- official/
 |   +-- git-enhanced/        # Rich git rendering
 |   +-- docker-dashboard/    # Container management
 |   +-- k8s-navigator/       # Kubernetes TUI
 |   +-- api-inspector/       # HTTP response rendering
 |   +-- markdown-renderer/   # Rich markdown display
 +-- community/
 |   +-- spotify-controls/    # Now playing + controls
 |   +-- github-notifications/# PR reviews, issues
 |   +-- crypto-ticker/       # Live price feeds
 |   +-- weather-widget/      # Inline weather
 |   +-- pomodoro/            # Focus timer
 +-- custom/
     +-- your-plugin-here/    # Build whatever you want


 PLUGIN API:
 +--------------------------------------------------------------+
 |  phantom::plugin::register("my-plugin", |ctx| {              |
 |      // React to commands                                    |
 |      ctx.on_command("git *", |cmd, output| {                 |
 |          // Upgrade git output to rich rendering             |
 |          render_git_enhanced(output)                          |
 |      });                                                     |
 |                                                              |
 |      // Add new commands                                     |
 |      ctx.register_command("deploy", |args| {                 |
 |          // Custom deploy workflow                            |
 |      });                                                     |
 |                                                              |
 |      // Add status bar segments                              |
 |      ctx.status_bar.add_segment(|| {                         |
 |          format!("  {}", get_now_playing())                 |
 |      });                                                     |
 |                                                              |
 |      // Add widgets                                          |
 |      ctx.register_widget("cpu-spark", || {                   |
 |          sparkline(get_cpu_history())                         |
 |      });                                                     |
 |  });                                                         |
 +--------------------------------------------------------------+
```

---

## THE STARTUP EXPERIENCE

```
 FRAME 0 (0.0s) - COLD BOOT:
 +=====================================================================+
 |                                                                      |
 |                          (black screen)                              |
 |                                                                      |
 |                  A single green cursor blinks.                       |
 |                          _                                           |
 |                                                                      |
 +=====================================================================+

 FRAME 1 (0.3s) - CRT WARMUP:
 +=====================================================================+
 |  ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~   |
 |  ~ CRT shader activates. Screen "warms up" with a phosphor glow ~  |
 |  ~ Scanlines fade in. A low hum (optional audio). The screen     ~  |
 |  ~ brightens from the center outward, like an old TV turning on. ~  |
 |  ~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~~   |
 +=====================================================================+

 FRAME 2 (0.8s) - LOGO:
 +=====================================================================+
 |                                                                      |
 |            ██████╗ ██╗  ██╗ █████╗ ███╗   ██╗████████╗              |
 |            ██╔══██╗██║  ██║██╔══██╗████╗  ██║╚══██╔══╝              |
 |            ██████╔╝███████║███████║██╔██╗ ██║   ██║                  |
 |            ██╔═══╝ ██╔══██║██╔══██║██║╚██╗██║   ██║                  |
 |            ██║     ██║  ██║██║  ██║██║ ╚████║   ██║                  |
 |            ╚═╝     ╚═╝  ╚═╝╚═╝  ╚═╝╚═╝  ╚═══╝   ╚═╝                  |
 |                                                                      |
 |                        v 0 . 1 . 0                                  |
 |                                                                      |
 +=====================================================================+

 FRAME 3 (1.2s) - SYSTEM CHECK:
 +=====================================================================+
 |                                                                      |
 |  [    ] Phantom Engine .......................... initializing       |
 |                                                                      |
 +=====================================================================+

 FRAME 4 (1.5s) - SYSTEM CHECK COMPLETE:
 +=====================================================================+
 |                                                                      |
 |  [ OK ] Phantom Engine .......................... online             |
 |  [ OK ] Context Engine .......................... 3 projects found   |
 |  [ OK ] Agent Runtime ........................... 5 slots ready      |
 |  [ OK ] Memory .................................. 847 entries loaded |
 |  [ OK ] Shader Pipeline ......................... CRT active         |
 |  [ OK ] Plugins ................................. 12 loaded          |
 |  [ OK ] Session ................................. restored (3 panes) |
 |                                                                      |
 |  Welcome back, jermiranda. Last session: 2 hours ago.               |
 |  You were working on: feature/agents in badass-cli                  |
 |                                                                      |
 |  3 unread GitHub notifications.                                     |
 |  CI is green. All 847 tests passing.                                |
 |                                                                      |
 |  jermiranda in badass-cli on  feature/agents                       |
 |  >>>                                                                |
 |                                                                      |
 +=====================================================================+
```

---

## THE COMPETITIVE LANDSCAPE

```
                    INTELLIGENCE
                         ^
                         |
                    P H A N T O M
                         |          (nobody is here)
                         |
                         |
                    Warp |  Wave
                         |
                         |
    RETRO ───────────────┼──────────────────> MODERN
                         |
                         |
         cool-retro-term |
                         |  Ghostty  Kitty  WezTerm
           Alacritty     |
                         |
              iTerm2     |  Terminal.app
                         |
                         v
                      DUMB


 Phantom occupies a space that DOES NOT EXIST YET.
 Modern + Intelligent + Beautiful.
 That's the gap. That's the opportunity.
```

---

## WHY FORK WEZTERM?

```
 OPTION A: Build from scratch
 +------------------------------------------+
 | Time to basic terminal emulation: 2 years |
 | VT100 compat, font rendering, GPU,       |
 | Unicode, BiDi text, clipboard, IME...    |
 | That's BEFORE we add any AI features.    |
 | Result: We die in the tarpit.            |
 +------------------------------------------+

 OPTION B: Fork WezTerm
 +------------------------------------------+
 | Terminal emulation: DONE (day 1)          |
 | GPU rendering: DONE (day 1)              |
 | Shader pipeline: DONE (day 1)            |
 | Multiplexing: DONE (day 1)              |
 | Lua scripting: DONE (day 1)             |
 | Image protocols: DONE (day 1)           |
 | Cross-platform: DONE (day 1)            |
 |                                          |
 | We spend our time on what MATTERS:       |
 | The AI layer. The agent system. The      |
 | semantic engine. The rich rendering.     |
 | The stuff that makes it PHANTOM.         |
 +------------------------------------------+

 WezTerm is MIT licensed. Fork it. Rename it. Build on it.
 Newton stood on the shoulders of giants. So do we.
```

**What we keep from WezTerm:**
- Terminal emulation core (VT100, xterm, ECMA-48)
- GPU rendering pipeline (OpenGL)
- Font rendering and shaping
- Input handling (keyboard, mouse, IME)
- Base multiplexer (tabs, panes)
- Lua configuration layer

**What we rip out / replace:**
- Default UI chrome -> Phantom's custom UI
- Basic status bar -> Phantom's rich status system
- Simple tab bar -> Context-aware workspace switcher

**What we ADD:**
- Semantic layer (command parsing engine)
- Agent runtime (sandboxed AI agent orchestrator)
- Rich rendering layer (mixed terminal + widget rendering)
- Context engine (project/env awareness)
- Memory system (persistent cross-session knowledge)
- Natural language interpreter
- WASM plugin system
- Reactive shader system
- Startup sequence engine

---

## TECH STACK

```
 LANGUAGE: Rust (primary) + Lua (config/scripting) + WASM (plugins)

 CORE:
 +------------------------------------------+
 | wezterm-core (forked)     Terminal emu    |
 | wgpu / OpenGL             GPU rendering   |
 | mlua                      Lua scripting   |
 | wasmtime                  WASM plugins    |
 +------------------------------------------+

 AI LAYER:
 +------------------------------------------+
 | Claude API / local models  LLM backbone  |
 | Custom semantic parser     Output parser  |
 | sled / SQLite              Memory store   |
 | tokio                      Async runtime  |
 +------------------------------------------+

 RENDERING:
 +------------------------------------------+
 | GLSL shaders               Post-process  |
 | Custom widget renderer     Rich content  |
 | Sixel + Kitty protocol     Images        |
 | Tree-sitter                Syntax parse  |
 +------------------------------------------+

 AGENT SYSTEM:
 +------------------------------------------+
 | Sandboxed shell sessions   Agent runtime |
 | Tool use framework         Agent tools   |
 | Message passing            Agent comms   |
 | Priority queue             Task mgmt     |
 +------------------------------------------+
```

---

## PROJECT STRUCTURE

```
 phantom/
 +-- crates/
 |   +-- phantom-core/           # Forked WezTerm terminal core
 |   +-- phantom-render/         # GPU rendering + shader pipeline
 |   +-- phantom-semantic/       # Command output parser
 |   +-- phantom-agents/         # Agent runtime & orchestration
 |   +-- phantom-context/        # Project/env awareness engine
 |   +-- phantom-memory/         # Persistent memory system
 |   +-- phantom-nlp/            # Natural language command interpreter
 |   +-- phantom-plugins/        # WASM plugin host
 |   +-- phantom-widgets/        # Rich content renderer (tables, charts, etc.)
 |   +-- phantom-ui/             # Chrome, status bar, workspace switcher
 |   +-- phantom-config/         # Lua config + themes
 +-- shaders/
 |   +-- crt.glsl
 |   +-- bloom.glsl
 |   +-- reactive.glsl
 |   +-- ambient.glsl
 +-- themes/
 |   +-- phosphor.lua            # Green CRT (default)
 |   +-- amber.lua               # Amber CRT
 |   +-- ice.lua                 # Cool blue / TRON
 |   +-- blood.lua               # Red / Cyberpunk
 |   +-- vapor.lua               # Vaporwave / Retrowave
 |   +-- solar.lua               # Solarized
 +-- plugins/
 |   +-- git-enhanced/
 |   +-- docker-dashboard/
 |   +-- spotify/
 +-- assets/
 |   +-- fonts/
 |   +-- ascii-art/
 |   +-- boot-sequence/
 +-- docs/
 |   +-- architecture.md
 |   +-- plugin-api.md
 |   +-- shader-api.md
 +-- Cargo.toml
 +-- README.md
 +-- LICENSE                     # MIT (inherited from WezTerm)
```

---

## DEVELOPMENT PHASES

```
 PHASE 0: FOUNDATION (Weeks 1-4)                    << START HERE
 +------------------------------------------------------+
 | - Fork WezTerm                                       |
 | - Strip down to core, rename to Phantom              |
 | - Custom boot sequence                               |
 | - CRT shader + theme engine                          |
 | - Custom UI chrome (tab bar, status bar)             |
 | - Ship as: "a beautiful terminal with retro shaders" |
 | - THIS IS ALREADY COOL ENOUGH TO SHOW PEOPLE.        |
 +------------------------------------------------------+
         |
         v
 PHASE 1: SEMANTIC LAYER (Weeks 5-8)
 +------------------------------------------------------+
 | - Command output parser (git, cargo, docker, etc.)   |
 | - Structured command history                          |
 | - Error detection and highlighting                   |
 | - Basic rich rendering (JSON, tables)                |
 | - Ship as: "a terminal that understands your output" |
 +------------------------------------------------------+
         |
         v
 PHASE 2: AGENTS (Weeks 9-16)                        << GAME CHANGER
 +------------------------------------------------------+
 | - Agent runtime (sandboxed shell sessions)           |
 | - Claude API integration                             |
 | - Agent pane rendering                               |
 | - Auto-detect errors -> suggest agent                |
 | - Basic agent tools (file read/write, git, shell)    |
 | - Ship as: "a terminal with AI agents built in"      |
 | - THIS IS WHERE WE BREAK THE INTERNET.               |
 +------------------------------------------------------+
         |
         v
 PHASE 3: CONTEXT & MEMORY (Weeks 17-20)
 +------------------------------------------------------+
 | - Project detection and awareness                    |
 | - Persistent per-project memory                      |
 | - Session save/restore with context                  |
 | - Natural language command interpretation             |
 +------------------------------------------------------+
         |
         v
 PHASE 4: ECOSYSTEM (Weeks 21+)
 +------------------------------------------------------+
 | - WASM plugin system                                 |
 | - Plugin marketplace                                 |
 | - Community themes and shaders                       |
 | - Documentation and tutorials                        |
 | - WORLD DOMINATION.                                  |
 +------------------------------------------------------+
```

---

## THE VISION STATEMENT

```
 +=====================================================================+
 |                                                                      |
 |  Phantom is not a terminal with AI bolted on.                       |
 |                                                                      |
 |  It's an AI system with a terminal built in.                        |
 |                                                                      |
 |  The difference matters.                                            |
 |                                                                      |
 |  When AI is a feature, it's optional. You can ignore it.            |
 |  When AI is the foundation, everything is smarter.                  |
 |  Every output is understood. Every error is caught.                 |
 |  Every workflow is enhanced. Not because you asked.                 |
 |  Because the terminal KNOWS.                                        |
 |                                                                      |
 |  We didn't add AI to a terminal.                                    |
 |  We built a terminal around AI.                                     |
 |                                                                      |
 |  That's Phantom.                                                    |
 |                                                                      |
 +=====================================================================+
```

---

*"Your terminal shouldn't just display text. It should understand it,
act on it, remember it, and render it beautifully. Anything less is
a dumb pipe with good font rendering."*

--- 

**Open source. MIT licensed. Built by hackers, for hackers.**

**This is Phantom. And it doesn't exist yet. Let's build it.**
