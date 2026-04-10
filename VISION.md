# BADASS-CLI: The Vision

```
+=====================================================================================+
|                                                                                     |
|   ____    _    ____    _    ____ ____       ____ _     ___                           |
|  | __ )  / \  |  _ \  / \  / ___/ ___|     / ___| |   |_ _|                         |
|  |  _ \ / _ \ | | | |/ _ \ \___ \___ \    | |   | |    | |                          |
|  | |_) / ___ \| |_| / ___ \ ___) |__) |   | |___| |___ | |                          |
|  |____/_/   \_\____/_/   \_\____/____/     \____|_____|___|                         |
|                                                                                     |
|                    "Not a dotfiles repo. A command center."                          |
|                                                                                     |
+=====================================================================================+
```

---

## THE LAYERS

This isn't "install some tools and slap a theme on it."
This is a fully integrated hacker command center with 5 layers:

```
 +---------------------------------------------------------------------------+
 |  LAYER 5: CUSTOM APPS                                                     |
 |  Hand-built TUI tools (Go/Bubble Tea + Rust/Ratatui)                      |
 |  Your own CLI toolkit that nobody else has                                |
 +---------------------------------------------------------------------------+
 |  LAYER 4: DASHBOARD & MONITORING                                          |
 |  Live system stats, git activity, weather, crypto, spotify                |
 |  All in a tmux layout that launches on boot                               |
 +---------------------------------------------------------------------------+
 |  LAYER 3: SHELL EXPERIENCE                                                |
 |  zsh + starship prompt + aliases + keybinds + fzf workflows               |
 |  Every command is faster, prettier, and smarter                           |
 +---------------------------------------------------------------------------+
 |  LAYER 2: MULTIPLEXER                                                     |
 |  tmux with custom status bar, session manager, smart layouts              |
 |  One keypress to launch your full dev environment                         |
 +---------------------------------------------------------------------------+
 |  LAYER 1: TERMINAL                                                        |
 |  WezTerm with CRT shaders, retro fonts, dynamic backgrounds              |
 |  The glass you look through sets the whole vibe                           |
 +---------------------------------------------------------------------------+
```

---

## LAYER 1: WEZTERM - THE GLASS

Not just a terminal. A portal.

```
 +=========================================================================+
 |  ///  WEZTERM  ///                                          [_][O][X]   |
 |=========================================================================|
 |                                                                         |
 |  +---CRT SHADER ACTIVE-------------------------------------------+     |
 |  |                                                                |     |
 |  |  > scanlines: subtle green phosphor lines across screen        |     |
 |  |  > barrel distortion: slight screen curvature at edges         |     |
 |  |  > chromatic aberration: RGB split on text edges               |     |
 |  |  > bloom/glow: bright text bleeds light like a real CRT        |     |
 |  |  > vignette: edges of screen slightly darkened                 |     |
 |  |  > noise: subtle film grain overlay                            |     |
 |  |                                                                |     |
 |  +----------------------------------------------------------------+     |
 |                                                                         |
 |  FONT: JetBrains Mono Nerd Font (ligatures + icons)                     |
 |  FALLBACK: Symbols Nerd Font Mono (for missing glyphs)                  |
 |                                                                         |
 |  COLOR SCHEME: Custom "Phosphor" theme                                  |
 |  +-------------------------------+                                      |
 |  |  bg:     #0a0e14  (deep void) |  Normal     Bright                   |
 |  |  fg:     #39ff14  (matrix grn)|  --------   --------                 |
 |  |  cursor: #39ff14  (glow)      |  black   0  brblack   8             |
 |  |  sel_bg: #1a3a1a  (dim green) |  red     1  brred     9             |
 |  |  sel_fg: #39ff14              |  green   2  brgreen  10             |
 |  +-------------------------------+  yellow  3  bryellow 11             |
 |                                     blue    4  brblue   12             |
 |  DYNAMIC FEATURES:                  magenta 5  brmagent 13             |
 |  > Tab bar styled as retro menu     cyan    6  brcyan   14             |
 |  > Right-click context menu         white   7  brwhite  15             |
 |  > Background changes by time                                           |
 |    (dark at night, amber at dawn)                                       |
 |  > Keybinds for shader toggle                                           |
 |  > Keybind to screenshot terminal                                       |
 |                                                                         |
 +=========================================================================+
```

---

## LAYER 2: TMUX - THE GRID

Your workspace, your rules.

```
 DEFAULT LAYOUT: "command-center"
 +==========================================================================+
 | [  badass  ] | 1:code | 2:term | 3:monitor | 4:git | 5:music |  s0    |
 +==========================================================================+
 |                                          |                               |
 |                                          |  $ git log --oneline -5       |
 |                                          |  a1b2c3d refactor auth        |
 |         MAIN EDITOR / WORK              |  d4e5f6g add dashboard         |
 |              PANE                         |  h7i8j9k fix shader bug       |
 |           (biggest)                       |                               |
 |                                          |-------------------------------+
 |                                          |                               |
 |                                          |  ~/Projects/badass-cli        |
 |                                          |   src/  config/  scripts/    |
 +------------------------------------------+   15 files, 3 dirs            |
 |                                          |                               |
 |  SECONDARY TERMINAL                      +-------------------------------+
 |  (tests, builds, logs)                   |  CPU [||||||||    ] 42%       |
 |                                          |  MEM [||||||      ] 31%       |
 |                                          |  DSK [||||||||||| ] 67%       |
 +==========================================================================+
 | SESSION: dev | #W: 5 | CPU: 42% |  4.2G | 🎵 Lo-Fi Beats | 72F |11PM |
 +==========================================================================+

 STATUS BAR SEGMENTS:
 +--------+--------+-----------+----------+--------+---------+------+------+
 |session |windows | cpu/mem   | battery  | git    | spotify | wthr | time |
 |  name  | count  | sparkline | icon+%   | branch | track   | temp | 24h  |
 +--------+--------+-----------+----------+--------+---------+------+------+

 KEYBINDS:
 prefix = Ctrl-a (not Ctrl-b, easier to reach)
 prefix + d = detach
 prefix + c = new window
 prefix + | = vertical split (intuitive)
 prefix + - = horizontal split (intuitive)
 prefix + h/j/k/l = vim-style pane navigation
 prefix + H/J/K/L = resize panes
 prefix + Tab = last window
 prefix + C = "command-center" layout (launches the full dashboard)
 prefix + G = lazygit popup
 prefix + F = fzf file finder popup
```

---

## LAYER 3: SHELL EXPERIENCE

Every keystroke is optimized.

```
 THE PROMPT (Starship):

 NORMAL:
 +---------------------------------------------------------------------------+
 |                                                                           |
 |   jermiranda in mass-cli on  main [!?] via  v21.1 took 3s           |
 |   at mass-cli  >>>                                                      |
 |                                                                           |
 +---------------------------------------------------------------------------+
 
 SEGMENTS:
 [user] [in] [directory] [on] [git branch] [git status] [via] [lang] [duration]
 [newline]
 [at] [project] [character >>>]

 PREVIOUS COMMANDS (transient - collapse to minimal):
 +---------------------------------------------------------------------------+
 |   >>> ls -la                                                              |
 |   (output here)                                                           |
 |   >>> git status                                                          |
 |   (output here)                                                           |
 |   jermiranda in badass-cli on  main via  v21.1                       |
 |   at badass-cli  >>>  <cursor>                                          |
 +---------------------------------------------------------------------------+

 TOOL REPLACEMENTS:
 +------------------+------------------+--------------------------------------+
 | OLD              | NEW              | WHY                                  |
 +------------------+------------------+--------------------------------------+
 | ls               | eza              | icons, colors, git status, tree      |
 | cat              | bat              | syntax highlighting, line numbers    |
 | find             | fd               | 10x faster, intuitive syntax         |
 | grep             | ripgrep (rg)     | 100x faster, respects .gitignore     |
 | cd               | zoxide (z)       | learns your habits, fuzzy jump       |
 | diff             | delta            | syntax-highlighted git diffs         |
 | du               | dust             | visual disk usage bars               |
 | ps               | procs            | colored, searchable process list     |
 | top/htop         | btop             | sci-fi system dashboard              |
 | man              | tldr             | practical examples, not novels       |
 | curl debug       | httpie / curlie  | human-friendly HTTP                  |
 +------------------+------------------+--------------------------------------+

 FZF WORKFLOWS:
 Ctrl+R  = fuzzy search command history (with preview)
 Ctrl+T  = fuzzy find files (with bat preview)
 Alt+C   = fuzzy cd into directories
 **<tab> = fuzzy completion for everything
```

---

## LAYER 4: DASHBOARD & MONITORING

Launch with one command: `badass-dashboard`

```
 +==========================================================================+
 |                    BADASS COMMAND CENTER v1.0                             |
 +==========================================================================+
 |  SYSTEM                    |  GIT ACTIVITY             |  WEATHER       |
 |  +-----------------------+ |  +---------------------+  | +------------+ |
 |  | CPU |||||||||| 67%    | |  |  12 commits today   |  | | SF  72F   | |
 |  | MEM ||||||    38%    | |  |   3 PRs open         |  | | Partly    | |
 |  | SWP ||         4%    | |  |   1 review pending   |  | | Cloudy    | |
 |  | GPU |||||||||  62%    | |  |  main: 2 ahead      |  | |     .--.  | |
 |  +-----------------------+ |  +---------------------+  | |  .-(    ).| |
 |  NET:  45 Mbps  12 Mbps | |                           | | (___.__)__)| |
 |  DISK: 234G / 500G       |  DOCKER                    | +------------+ |
 |                           |  +---------------------+  |                 |
 |  PROCESSES (top 5)        |  | nginx     UP  2d    |  |  SPOTIFY       |
 |  +---------------------+  |  | postgres  UP  2d    |  | +------------+ |
 |  | node     12.3% 1.2G |  |  | redis     UP  2d    |  | | Now:       | |
 |  | chrome    8.1% 3.4G |  |  | app       UP  45m   |  | | Synthwave  | |
 |  | docker    4.2% 0.8G |  |  +---------------------+  | | Retro Mix  | |
 |  | code      3.1% 1.1G |  |                           | | >> |||     | |
 |  | postgres  1.8% 0.4G |  |  CRYPTO                   | +------------+ |
 |  +---------------------+  |  +---------------------+  |                 |
 |                           |  | BTC  $98,432  +2.1% |  |  QUOTE OF DAY  |
 |  NETWORK MAP             |  | ETH   $3,891  -0.4% |  | +------------+ |
 |  +---------------------+  |  | SOL     $187  +5.2% |  | |"Any suffic-| |
 |  | 192.168.1.x         |  |  +---------------------+  | |iently adv. | |
 |  | [*]--[hub]--[net]   |  |                           | |technology is| |
 |  |       |             |  |  CALENDAR                 | |indistinguish| |
 |  |     [nas]           |  |  +---------------------+  | |able from    | |
 |  +---------------------+  |  | 10:00 Standup       |  | |magic."      | |
 |                           |  | 14:00 Code Review   |  | |  - Clarke   | |
 |                           |  | 16:00 Deploy window |  | +------------+ |
 +==========================================================================+
```

---

## LAYER 5: CUSTOM CLI APPS

This is the "nobody else has this" layer.

### APP 1: `badass` - Your Meta CLI Tool
```
 $ badass

 ____    _    ____    _    ____ ____
 | __ )  / \  |  _ \  / \  / ___/ ___|
 |  _ \ / _ \ | | | |/ _ \ \___ \___ \
 | |_) / ___ \| |_| / ___ \ ___) |__) |
 |____/_/   \_\____/_/   \_\____/____/

 COMMANDS:
   badass init          Set up a new machine from scratch
   badass dashboard     Launch the command center
   badass theme         Switch color themes (phosphor/amber/ice/blood)
   badass backup        Backup all configs to git
   badass restore       Restore configs from git
   badass update        Update all CLI tools
   badass status        Show system + project status
   badass hack          Toggle "hacker mode" (CRT shader + cmatrix bg)
   badass zen           Minimal mode (clean prompt, no distractions)
   badass record        Start recording terminal session (VHS)
   badass fetch         Show system info (fastfetch + custom ASCII)
```

### APP 2: `proj` - Smart Project Manager TUI
```
 +==========================================================================+
 |  PROJ - Project Manager                                    Ctrl+Q quit  |
 +==========================================================================+
 |  PROJECTS              |  DETAILS                                       |
 |  +-------------------+ |  +-------------------------------------------+ |
 |  | > badass-cli      | |  | badass-cli                                | |
 |  |   my-saas-app     | |  | ~/Documents/GitHub/badass-cli             | |
 |  |   dotfiles        | |  |                                           | |
 |  |   side-project    | |  | Branch: main                              | |
 |  |   work-api        | |  | Last commit: 2h ago                       | |
 |  +-------------------+ |  | Status: 3 modified, 1 untracked           | |
 |                        |  |                                           | |
 |  ACTIONS               |  | RECENT ACTIVITY:                         | |
 |  [o] Open in editor    |  | > 2h  refactor: shell config             | |
 |  [t] Open terminal     |  | > 5h  feat: add CRT shader               | |
 |  [g] Open lazygit      |  | > 1d  init: project setup                | |
 |  [d] Open dashboard    |  +-------------------------------------------+ |
 |  [n] New project       |                                               |
 +==========================================================================+
```

### APP 3: `matrix` - Screensaver / Lock Screen
```
 +==========================================================================+
 |                                                                          |
 |  1 0 1 1 0 0 1  ENTER PASSPHRASE:  0 1 1 0 1 0 0                       |
 |  0 1 0 0 1 1 0  > ********         1 0 0 1 1 0 1                       |
 |  1 1 0 1 0 1 1                     0 1 1 0 0 1 0                       |
 |  0 0 1 0 1 0 0  [AUTHENTICATING..] 1 1 0 1 0 0 1                       |
 |  1 0 1 1 0 0 1                     0 0 1 0 1 1 0                       |
 |  0 1 0 0 1 1 0  ACCESS GRANTED     1 0 1 1 0 0 1                       |
 |                                                                          |
 +==========================================================================+
 
 > Falling matrix rain in background
 > ASCII art logo fades in
 > Fake "authentication" sequence plays
 > Drops you into your tmux session
```

### APP 4: `sysmon` - Custom System Monitor
```
 Built with Ratatui (Rust) or Bubble Tea (Go)
 Like btop but YOUR aesthetic, YOUR layout, YOUR data
 
 Could include:
 - CPU/RAM/GPU with custom sparkline characters
 - Network throughput graph
 - Docker container status
 - Active tmux sessions
 - Git repo statuses across all projects
 - Custom alerts (disk full, high CPU, etc.)
```

---

## THE STARTUP SEQUENCE

When you open WezTerm, this happens:

```
 FRAME 1 (0.0s):
 +------------------------------------------+
 |                                           |
 |  CRT shader flickers on                  |
 |  Screen "warms up" like an old monitor   |
 |                                           |
 +------------------------------------------+

 FRAME 2 (0.5s):
 +------------------------------------------+
 |                                           |
 |  ████████████████████████████████████████ |
 |  ██                                    ██ |
 |  ██   B A D A S S   C L I   v 1 . 0   ██ |
 |  ██                                    ██ |
 |  ████████████████████████████████████████ |
 |                                           |
 |  Initializing...                          |
 +------------------------------------------+

 FRAME 3 (1.0s):
 +------------------------------------------+
 |  [OK] WezTerm loaded                      |
 |  [OK] tmux session restored               |
 |  [OK] 14 tools online                     |
 |  [OK] System nominal                      |
 |                                           |
 |  Type 'badass' for commands               |
 |  Type 'badass dashboard' for full view    |
 |                                           |
 |  jermiranda in ~ on  main                |
 |  at home  >>>                            |
 +------------------------------------------+
```

---

## THEMES (Switchable with `badass theme`)

```
 PHOSPHOR (default)          AMBER                      ICE
 bg: #0a0e14                 bg: #1a1000                bg: #0a0e1a
 fg: #39ff14                 fg: #ffb000                fg: #00d4ff
 accent: #00ff41             accent: #ff8c00            accent: #0099ff
 vibe: Matrix                vibe: Fallout terminal     vibe: TRON

 BLOOD                       VAPOR                      SOLAR
 bg: #1a0000                 bg: #1a0a2e                bg: #002b36
 fg: #ff0040                 fg: #ff71ce                fg: #839496
 accent: #ff0000             accent: #b967ff            accent: #b58900
 vibe: Cyberpunk             vibe: Vaporwave/Retrowave  vibe: Solarized Dark
```

---

## PROJECT STRUCTURE

```
 badass-cli/
 +-- wezterm/                    # WezTerm config + shaders
 |   +-- wezterm.lua             # Main config
 |   +-- shaders/
 |   |   +-- crt.glsl            # CRT effect shader
 |   |   +-- bloom.glsl          # Glow/bloom shader
 |   +-- themes/
 |       +-- phosphor.lua
 |       +-- amber.lua
 |       +-- ice.lua
 +-- tmux/                       # tmux config + plugins
 |   +-- tmux.conf
 |   +-- layouts/
 |   |   +-- command-center.sh
 |   |   +-- dev.sh
 |   +-- scripts/
 |       +-- spotify.sh
 |       +-- weather.sh
 |       +-- crypto.sh
 +-- shell/                      # zsh config
 |   +-- zshrc
 |   +-- aliases.zsh
 |   +-- functions.zsh
 |   +-- keybinds.zsh
 +-- starship/                   # Starship prompt config
 |   +-- starship.toml
 +-- tools/                      # Tool configs
 |   +-- bat.conf
 |   +-- delta.gitconfig
 |   +-- lazygit.yml
 |   +-- btop.conf
 |   +-- yazi/
 |   +-- fastfetch/
 +-- apps/                       # CUSTOM CLI APPS
 |   +-- badass-meta/            # The 'badass' command (Go or Rust)
 |   +-- proj/                   # Project manager TUI
 |   +-- sysmon/                 # Custom system monitor
 |   +-- matrix-login/           # Matrix login screen
 +-- scripts/                    # Setup & utility scripts
 |   +-- install.sh              # One-command full setup
 |   +-- backup.sh
 |   +-- restore.sh
 |   +-- startup-sequence.sh     # The boot animation
 +-- assets/                     # ASCII art, fonts, images
 |   +-- ascii/
 |   |   +-- logo.txt
 |   |   +-- banner.txt
 |   +-- wallpapers/
 +-- README.md
 +-- Makefile                    # make install, make update, etc.
```

---

## TECH STACK FOR CUSTOM APPS

```
 Option A: Go + Charm ecosystem
 +-------------------------------------------+
 | Framework: Bubble Tea                      |
 | Styling:   Lip Gloss                       |
 | Forms:     Huh                             |
 | Scripting: Gum                             |
 | Pros: Fast to build, great ecosystem       |
 | Cons: Slightly less control than Rust      |
 +-------------------------------------------+

 Option B: Rust + Ratatui
 +-------------------------------------------+
 | Framework: Ratatui                         |
 | Async:     Tokio                           |
 | CLI:       Clap                            |
 | Pros: Maximum performance, type safety     |
 | Cons: Slower to develop                    |
 +-------------------------------------------+

 Recommendation: Go for the meta tools (badass, proj)
                 Rust for the performance tools (sysmon)
                 Shell scripts for glue (startup, tmux layouts)
```

---

## WHAT MAKES THIS "NOT HALF-ASSED"

1. **Custom GLSL shaders** - Not just a color scheme. Actual GPU shaders for CRT effects.
2. **Custom-built CLI apps** - Not just installing other people's tools. YOUR tools.
3. **Startup sequence** - A boot animation. Because why not.
4. **Theme engine** - Switch entire aesthetic with one command.
5. **One-command setup** - `make install` on a fresh Mac and everything works.
6. **Dashboard** - A live command center, not just a pretty prompt.
7. **Session management** - tmux sessions that persist and restore.
8. **Everything is version controlled** - Clone this repo on any Mac and you're home.

---

*"We're not installing some shit. We're building a command center."*
