# Agent Bar

A GNOME Shell top-bar indicator for AI coding subscriptions: how much of your
**Claude** and **ChatGPT** plan limits is left, when they reset, and how many tokens
you've used (with an API-equivalent cost estimate) across Claude Code, Codex, pi and
opencode.

```
top bar:  [C] 74% · 2h13m   [O] 36% · 1h30m
menu:     [ Claude 74% ][ ChatGPT 36% ]
          5-hour ▓▓▓▓▓▓▓░░  74% left · resets in 2h13m
          Weekly ▓▓▓░░░░░░  31% left · resets in 4d15h
          Last 24 hours · est. cost   ▁▂▅▃▁▇█▂
          24h | 7d | 30d   Est. cost $5.77 · Tokens 12.4M
          By model · By tool · This week by product
```

It has two parts:

- **`agent-bar-daemon`** (Rust, systemd user service) reads your agents' local logs and
  polls each plan's usage endpoint, then publishes a JSON snapshot on the session bus
  (`dev.agentbar.Usage`).
- **The `agent-bar@local` extension** (GJS) only draws what the daemon publishes.

## Requirements

- GNOME Shell 50 (developed on Fedora 44, Wayland)
- Rust toolchain (`cargo`); [`just`](https://github.com/casey/just) for the recipes
- At least one of the following signed in on this machine:
  - **Claude:** [Claude Code](https://claude.com/claude-code) with a Pro/Max subscription
  - **ChatGPT:** [Codex CLI](https://github.com/openai/codex) signed in with ChatGPT

## Install

```sh
just install
```

This builds the daemon into `~/.local/bin`, installs and starts the
`agent-bar.service` user unit, copies the example config to
`~/.config/agent-bar/config.toml` (if none exists), and installs and enables the
extension. On Wayland, **log out and back in** once so GNOME Shell loads the extension.

Other recipes: `just status`, `just logs`, `just uninstall`.

### Logos (optional)

The top bar shows each provider's logo if you supply one; otherwise it shows a
progress ring. Put single-color SVGs in `extension/agent-bar@local/icons/` as
`claude-symbolic.svg` and `openai-symbolic.svg` before `just install`. They are not
included because they're the owners' trademarks; see
[`icons/README.md`](extension/agent-bar@local/icons/README.md).

## What it reads

| Source | Used for | Location |
| --- | --- | --- |
| Claude usage endpoint | Claude limits | `api.anthropic.com/api/oauth/usage` with Claude Code's token |
| ChatGPT usage endpoint | ChatGPT limits | `chatgpt.com/backend-api/wham/usage` with Codex's token |
| Claude Code | Claude usage | `~/.claude/projects/**/*.jsonl` (`$CLAUDE_CONFIG_DIR`) |
| Codex CLI | ChatGPT usage | `~/.codex/sessions/**/rollout-*.jsonl` (`$CODEX_HOME`) |
| pi | Claude/ChatGPT usage | `~/.pi/agent/sessions/**/*.jsonl` (`$PI_CODING_AGENT_DIR`) |
| opencode | Claude/ChatGPT usage | `~/.local/share/opencode/opencode.db` (read-only) |

- **Only subscription usage is counted.** API-key and free-model usage is skipped.
  Codex usage is classified per request; pi by its current sign-in (`auth.json`
  providers with `type: oauth`); opencode by rows with zero billed cost.
- **Credentials stay where they are.** Tokens are read from each tool's own credentials
  file, kept in memory only, never logged, and **never refreshed** by the daemon (that
  could sign the tool out). When a token expires, the account shows as stale until you
  run Claude Code or `codex` again.
- **Gentle on the endpoints.** At most one request per minute per endpoint (manual
  refreshes included), with exponential backoff on 429/5xx.
- **The bars are the provider's numbers.** pi and opencode use a separate ChatGPT login
  that the usage endpoint doesn't accept, so ChatGPT bars come from Codex's sign-in; pi
  and opencode usage still appears in the stats.
- **Costs are estimates:** what the tokens would cost at API list prices, as of
  2026-10. Override prices in the config file.

The service runs sandboxed (`ProtectHome=read-only`); its only writable path is
opencode's data directory, which SQLite needs for reading a database in WAL mode.

## Configure

- **Extension settings** (menu → Settings): what's left vs. used, which accounts and
  which limit appear in the top bar, where the reset countdown appears, logo vs. ring,
  hiding an account, and low-quota notification thresholds.
- **Daemon config** (`~/.config/agent-bar/config.toml`): polling intervals and price
  overrides. See [`packaging/config.example.toml`](packaging/config.example.toml), then
  `systemctl --user restart agent-bar`.

## Develop

```sh
just test         # rustfmt check, cargo tests, extension unit tests, headless Shell test
just run-daemon   # daemon in the foreground (stop the service first)
just devkit       # nested GNOME Shell window with the extension and a debug daemon
agent-bar-daemon --print   # one snapshot as JSON
```

`scripts/headless-test.sh` runs the extension in an isolated headless GNOME Shell
(private session bus, in-memory settings) against fake data and local stand-ins for
both usage endpoints, so tests never touch your real logs or the real APIs.

The design notes, data-format findings and open items are in [`PLAN.md`](PLAN.md).

## Status

Personal project, version 0.1.0. The usage endpoints are undocumented and may change
without notice.

## License

[MIT](LICENSE). Provider names and logos belong to their owners and aren't covered by
this license.
