# triagent

**A coding agent in your terminal that uses your existing AI subscriptions** —
Claude (claude.ai), ChatGPT (Plus/Pro) and Google Gemini (Antigravity) — with
**no API keys**. It logs in through your browser exactly like the official CLIs
do, then gives you a full-screen agent that can read and edit files, run
commands, and use 89 bundled skills.

- 🖥️ **Terminal UI** like Claude Code / OpenCode: transcript, command menu, themes
- 🔐 **Browser login**, tokens stored locally in a 0600 file
- 🧰 **Real agent tools**: bash, read/write/edit files, glob, grep, web fetch,
  background shells, subagents
- 🧠 **89 skills** ready to use (`/skills`, then `/ponytail`, `/seo-audit`, …)
- 🚀 **Nothing runs in the background** — close the window and it's gone

> ⚠️ This is a personal-use tool. It uses consumer subscription endpoints that
> the providers do not officially endorse for third-party clients. Use your own
> accounts, and expect endpoints to change occasionally.

---

## 1. What you need

| | |
|---|---|
| **Computer** | Linux (tested on Linux Mint / Ubuntu). Terminal with 256 colors. |
| **Account** | At least one of: Claude Pro/Max · ChatGPT Plus/Pro · Google Antigravity (Gemini). |
| **Internet** | For building once, and for the agent to talk to the provider. |

You do **not** need an API key from any provider.

---

## 2. Install (first time, ~5 minutes)

### Step 2.1 — Install Rust

Open a terminal and paste:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
```

### Step 2.2 — Build triagent

From the folder that contains this README:

```bash
cargo build --release
```

This takes 1–2 minutes and downloads the dependencies once.

### Step 2.3 — Put it on your PATH

```bash
install -m755 target/release/triagent ~/.local/bin/triagent
```

Make sure `~/.local/bin` is in your PATH (it usually is). Check:

```bash
triagent --version
```

### Step 2.4 — Install the skills (recommended)

The repo bundles 89 skills. Install them with one command:

```bash
./scripts/install-skills.sh
```

That copies them to `~/.config/triagent/skills/`, where triagent finds them
automatically.

If you see a version number and the skills line, you're done. 🎉

> Shortcut: `cargo install --path .` also installs it to `~/.cargo/bin`.

---

## 3. Log in (one time per provider)

Choose the provider you have a subscription for:

```bash
triagent login claude      # claude.ai subscription
triagent login chatgpt     # ChatGPT Plus/Pro
triagent login gemini      # Google Antigravity / Gemini
```

A browser window opens. Sign in normally and approve the app. When it redirects
to a "login complete" page, close the tab — the terminal prints a success line.

Your token is saved to `~/.local/share/triagent/credentials.json` (permissions
`0600`, readable only by you). It refreshes automatically.

---

## 4. Run it

```bash
triagent --provider gemini     # or claude / chatgpt
```

You get a full-screen interface:

```
 gemini · gemini-3.8-flash-high · default      ↑0 ↓0 · $0.00 · session-id
  ℹ triagent — Claude Code-style TUI
❯ Say hi in one line
Hello! How can I help you today?
──────────────────────────────────────────────────────────────
╭────────────────────────────────────────────────────────────╮
│ ❯                                                          │
╰────────────────────────────────────────────────────────────╯
```

Just type and press **Enter**. That's it — you're talking to your subscription.

### Keybindings

| Key | What it does |
|---|---|
| `Enter` | send message |
| `/` | open the command menu (type to filter, `↑`/`↓`, `Enter`) |
| `?` | help |
| `Shift+Tab` or `Tab` | cycle permission mode (default → accept-edits → plan → bypass) |
| `y` / `n` / `a` | when it asks "run command …?" (a = allow all this session) |
| `Ctrl+C` | interrupt the current answer (press twice to quit) |
| `Ctrl+D` | quit |
| `Ctrl+L` | redraw |
| `PgUp` / `PgDn` | scroll the transcript |
| `↑` / `↓` | input history |
| `!command` | run a shell command immediately |
| `Esc` | clear the input |

### One-shot mode (no full-screen)

```bash
triagent -p "explain this repository" --provider gemini
```

---

## 5. Everyday use

```bash
triagent                                  # use the default provider
triagent --provider gemini                # pick a provider
triagent --provider gemini --model gemini-3.8-flash-low
triagent --dangerously-skip-permissions   # don't ask before running commands
triagent --resume <session-id>            # continue a saved session
triagent status                           # login state and usage per provider
triagent models                           # list models for the current provider
```

### Slash commands

Open with `/` for the list, or type them directly:

| Command | What it does |
|---|---|
| `/help` | show all commands |
| `/model <id>` · `/models` | switch / list models |
| `/provider <id>` | switch provider |
| `/theme <name>` · `/themes` | change colors |
| `/status` · `/cost` | quota and token/cost totals |
| `/permissions` | show the current permission mode |
| `/skills` · `/skill <name>` | list and load skills |
| `/compact` | summarize the conversation to free context |
| `/undo` · `/rewind` | undo the last turn |
| `/new` | start a fresh session |
| `/sessions` · `/resume <id>` | list / load saved sessions |
| `/export` | save the session to Markdown |
| `/init` | create a project `TRIAGENT.md` |
| `/memory` | edit your global memory file |
| `/editor` | write a long message in `$EDITOR` |
| `/review` · `/security-review` · `/pr-comments` | review the current git diff |
| `/release-notes` | draft release notes from `git log` |
| `/details` · `/thinking` | toggle verbose tool output / reasoning display |
| `/prompts` | list your custom commands |
| `/doctor` | local health checks |
| `/bug` | info to include in a bug report |
| `/config` · `/pwd` | show config / working directory |
| `/login` · `/logout` | manage logins without leaving |
| `/clear` · `/exit` | reset / quit |

---

## 6. Skills

The repo bundles 89 skills (`./skills/`, installed by
`./scripts/install-skills.sh`) into `~/.config/triagent/skills/` — one folder
per skill, each with a `SKILL.md`. Each skill keeps its own license.

- The agent sees a compact index of all skills in its system prompt.
- `/skills` lists them.
- `/ponytail write a minimal Python add function` loads that skill and applies it.

Great ones to try: `/ponytail` (lazy/minimal code), `/frontend-design`,
`/seo-audit`, `/architect`, `/develop`, `/debug`, `/replica-recon`,
`/motion-designer`, `/document`.

Add more skills by dropping folders with a `SKILL.md` into
`~/.config/triagent/skills/`.

---

## 7. Themes

`/theme <name>` (persists to your config), `/themes` lists them:

`claude` · `opencode` · `system` · `tokyonight` · `gruvbox` · `nord` ·
`catppuccin` · `matrix` · `one-dark` · `everforest` · `ayu`

---

## 8. Configuration

`~/.config/triagent/config.toml` (created on first run):

```toml
[defaults]
provider = "claude"                 # claude | chatgpt | gemini
# model = "claude-sonnet-5"         # optional

[providers.claude]
default_model = "claude-sonnet-5"

[providers.chatgpt]
default_model = "gpt-5.3-codex"

[providers.gemini]
default_model = "gemini-3.8-flash-high"

[permissions]
default_mode = "default"            # default | accept-edits | plan | bypass

[ui]
theme = "claude"

[skills]
dirs = ["~/.config/triagent/skills"]
```

Built-in prompts can be extended with **custom commands**: create
`~/.config/triagent/commands/hello.md`:

```markdown
---
description: Say hello
---
Greet me in one sentence and mention $ARGUMENTS.
```

Then `/hello world` runs it.

---

## 9. Troubleshooting

**The browser didn't open when logging in.** Copy the URL that the terminal
printed into any browser. If you're over SSH, use the printed URL locally.

**"Port 1455/1457 busy" (ChatGPT login).** Close other login windows and retry.

**"no claude login".** Run `triagent login claude` first.

**Quota/rate-limit errors.** You've used the subscription limit — wait for the
reset (the error says when) or use another provider (`triagent --provider gemini`).

**The screen looks broken after quitting.** Run `reset` (or close the tab). The
agent restores the terminal on exit; a crash can leave it in raw mode.

**Text looks dim/hard to read.** Try `/theme opencode` or `/theme system`.

**Nothing runs after I close the window.** Correct — there is no daemon. That's
the design.

---

## 10. FAQ

**Is it safe?** It stores tokens in a `0600` file under `~/.local/share/triagent`.
It never sends your tokens anywhere except to the provider you're using, and it
does not run background services.

**Does it keep running?** No. One foreground process; closing the terminal ends it.

**Which subscriptions work?** claude.ai Pro/Max, ChatGPT Plus/Pro, and
Google Antigravity (Gemini). Free tiers may work but are untested.

**How do I update?** Pull the latest code and re-run steps 2.2–2.3.

**How do I uninstall?** `rm ~/.local/bin/triagent` and optionally
`rm -rf ~/.config/triagent ~/.local/share/triagent`.

---

## 11. Development

```bash
cargo test                 # 22 tests
cargo clippy --all-targets # clean
scripts/verify-no-background.sh   # proves nothing is left running
```

Project layout: `src/auth` (OAuth flows), `src/providers` (Claude/ChatGPT/Gemini
streaming), `src/agent` (loop, tools, permissions), `src/ui.rs` (TUI),
`src/theme.rs` (colors), `src/skills.rs` (skills).

---

## 12. License

MIT — see [LICENSE](LICENSE).
