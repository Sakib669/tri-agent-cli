# Reference

Detailed reference for every part of triagent. Start with
[QUICKSTART.md](QUICKSTART.md) if you just want to run it.

## CLI

```
triagent [OPTIONS] [COMMAND]

Commands:
  login <provider>    browser OAuth login (claude | chatgpt | gemini)
  logout <provider>   remove stored credentials (revokes ChatGPT remotely)
  status              login state, plan tier, usage per provider
  models [provider]   list models for a provider

Options:
  -p, --print <PROMPT>           one-shot mode (no full-screen UI)
      --provider <PROVIDER>      claude | chatgpt | gemini
      --model <MODEL>            model id for this run
      --dangerously-skip-permissions   never ask before running tools
      --resume <SESSION_ID>      continue a saved session
      --add-dir <PATH>           extra directory the agent may read/write
      --debug                    verbose logs (tokens are redacted)
```

## Permission modes (Shift+Tab / Tab)

| Mode | Behaviour |
|---|---|
| `default` | asks before shell commands and writes outside the workspace |
| `accept-edits` | same, but the model can edit files in the project without asking |
| `plan` | read-only tools; the model presents a plan via `exit_plan_mode` for approval |
| `bypass` | never asks (same as `--dangerously-skip-permissions`) |

Answers to a prompt: `y` yes · `n` no · `a` allow everything this session.

## Slash command reference

### General
| Command | Description |
|---|---|
| `/help` | all commands |
| `/exit` `/quit` `/q` | quit (session saved) |
| `/clear` | reset the conversation |
| `/new` | start a fresh session |
| `/compact` `/summarize` | summarize the conversation to free context |
| `/undo` `/rewind` | undo the last turn |
| `/redo` | redo (kept for parity; currently informational) |
| `/editor` | compose a message in `$EDITOR` |
| `/export` | write the session to `triagent-export-<session>.md` |
| `/sessions` | list saved sessions |
| `/resume <id>` | load a saved session |
| `/pwd` | show the working directory |
| `/init` | create a project `TRIAGENT.md` |
| `/memory` | edit the global memory file |

### Providers & models
| Command | Description |
|---|---|
| `/login <provider>` | browser login |
| `/logout <provider>` | remove login |
| `/connect <provider>` | alias of `/login` |
| `/provider <id>` | switch provider for this session |
| `/model <id>` | switch model |
| `/models` | list models |
| `/status` | usage/quota |
| `/cost` `/usage` | token and cost totals |

### UI
| Command | Description |
|---|---|
| `/theme <name>` | switch theme (persisted) |
| `/themes` | list themes |
| `/details` | toggle full tool output |
| `/thinking` | toggle reasoning display |
| `/output-style` | toggle detailed explanations |
| `/keybinds` | keybindings |

### Skills & prompts
| Command | Description |
|---|---|
| `/skills` | list installed skills |
| `/skill <name>` | load a skill |
| `/<skill-name>` | load that skill directly |
| `/prompts` | list custom commands |
| `/<custom-name> [args]` | run a custom command |

### Review & diagnostics
| Command | Description |
|---|---|
| `/review` | review the current git diff |
| `/security-review` | security-focused diff review |
| `/pr-comments` | suggest PR comments from the diff |
| `/release-notes` | draft release notes from `git log` |
| `/doctor` | local health checks |
| `/config` | show the config file |
| `/bug` | info to include in a bug report |
| `/upgrade` | show version info |
| `/todos` | info about the todo tool |
| `/agents` | info about subagents |

## Keybindings

| Key | Action |
|---|---|
| `Enter` | submit |
| `/` | command menu |
| `?` | help |
| `Shift+Tab` / `Tab` | cycle permission mode |
| `y` `n` `a` | answer a permission prompt |
| `Ctrl+C` | interrupt (twice = quit) |
| `Ctrl+D` | quit |
| `Ctrl+L` | redraw |
| `PgUp` / `PgDn` | scroll |
| `↑` / `↓` | history |
| `Esc` | clear input |
| `!cmd` | run a shell command |

## Files

| Path | Purpose |
|---|---|
| `~/.config/triagent/config.toml` | config |
| `~/.config/triagent/credentials.json` | tokens (0600) |
| `~/.config/triagent/skills/` | installed skills |
| `~/.config/triagent/commands/` | custom slash commands |
| `~/.config/triagent/TRIAGENT.md` | global memory |
| `~/.local/share/triagent/sessions/` | session history |

## Tools the agent can call

`bash` (incl. `run_in_background`), `bash_output`, `kill_shell`, `read_file`,
`write_file`, `edit_file`, `glob`, `grep`, `list_dir`, `todo`, `webfetch`,
`task` (subagent), `exit_plan_mode` (plan mode only).

## Environment variables

| Variable | Purpose |
|---|---|
| `TRIAGENT_GEMINI_BASE_URL` | override the Gemini backend host |
| `TRIAGENT_TASK_DEPTH` | internal: subagent nesting guard |
| `EDITOR` | editor used by `/editor` and `/memory` |

## Exit codes

`0` success · `1` error · `143` terminated by signal (window closed).
