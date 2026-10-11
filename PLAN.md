# agent-bar — plan

GNOME top-bar indicator showing AI agent subscription limits and usage stats.
Target: GNOME Shell 50 (Fedora 44). Personal use first; keep it publishable on
extensions.gnome.org (EGO) from day one.

## Decisions

- **Split:** Rust daemon does all I/O (HTTP, logs, credentials); the GNOME Shell
  extension (GJS) only renders. Nothing blocking or secret ever touches the Shell process.
- **IPC:** session D-Bus, name `dev.agentbar.Usage`, path `/dev/agentbar/Usage`.
  - `GetSnapshot() → s` (JSON), `Refresh()`, signal `Changed(s)`.
  - JSON payload so the schema can evolve without touching D-Bus signatures.
- **Providers:** Claude now, Codex next. A `Provider` trait from the start; each
  provider has *limits* (live quota windows) and *stats* (log-derived usage).
- **Stats:** tokens (input / output / cache read / cache write) and estimated cost,
  over ranges **1d, 7d, 30d**, plus per-model breakdown.
- **EGO-ready:** ESM, clean `disable()`, no blocking calls, GSettings for prefs,
  no bundled binaries in the extension zip (daemon is installed separately).

## Architecture

```
GNOME Shell ─ agent-bar@local (GJS)
  top bar:  ◔ 37% · 2h13m
  dropdown: limits w/ progress bars + reset timers, 1d/7d/30d tokens & cost, product breakdown
        ▲ Changed(json)            │ Refresh()
        │                          ▼
agent-bar-daemon (Rust: tokio, zbus, reqwest, notify, serde) — systemd --user
  ├─ claude::limits  → GET api.anthropic.com/api/oauth/usage
  ├─ claude::logs    → ~/.claude/projects/**/*.jsonl
  └─ codex::*        → (next) ~/.codex/sessions/**
```

## Repo layout

```
agent-bar/
├─ daemon/
│  ├─ src/main.rs            # runtime, scheduler, wiring
│  ├─ src/model.rs           # Snapshot, ProviderSnapshot, LimitWindow, UsageStats
│  ├─ src/dbus.rs            # zbus interface
│  ├─ src/pricing.rs         # per-model $/MTok table (overridable via config)
│  └─ src/providers/{mod.rs, claude/{limits.rs, logs.rs}}
│  └─ tests/fixtures/
├─ extension/agent-bar@local/{metadata.json, extension.js, prefs.js, stylesheet.css, schemas/}
├─ packaging/agent-bar.service
└─ justfile
```

## Snapshot schema (v1)

```json
{
  "version": 1,
  "updated_at": "…",
  "providers": [{
    "id": "claude", "name": "Claude", "plan": "pro",
    "status": "ok",                          // ok | stale | error
    "error": null,
    "limits": [
      {"id": "session",    "label": "5-hour", "used_pct": 37, "severity": "normal",
       "resets_at": "…", "is_active": true},
      {"id": "weekly_all", "label": "Weekly", "used_pct": 52, "severity": "warning",
       "resets_at": "…", "is_active": false}
    ],
    "breakdown": [{"label": "Claude Code", "pct": 70}, …],   // weekly share by product
    "stats": {
      "1d":  {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0,
              "total_tokens": 0, "cost_usd": 0.0, "requests": 0, "sessions": 0,
              "by_model": [{"model": "…", "total_tokens": 0, "cost_usd": 0.0}]},
      "7d":  {…},
      "30d": {…}
    }
  }]
}
```

## Phase 1 findings (2026-10-11)

### Limits endpoint — `GET https://api.anthropic.com/api/oauth/usage`
- Headers: `Authorization: Bearer <claudeAiOauth.accessToken>`, `anthropic-beta: oauth-2025-04-20`.
- **Undocumented / internal.** Response contains many codename keys that may come and go.
- Parse the generic **`limits[]`** array, not the named fields:
  `{kind, group, percent, severity, resets_at, scope, is_active}` — forward compatible,
  and `severity` gives us server-side colour bands.
- `seven_day_breakdown.rows[]` → weekly share per product (Claude Code / Chats / Cowork / Other).
- `extra_usage` / `spend` → show only if `enabled`.
- Credentials: `~/.claude/.credentials.json` → `claudeAiOauth.{accessToken, expiresAt(ms), subscriptionType, rateLimitTier}`.
  Read-only, in memory, never logged. **Never refresh** — if `expiresAt` passed, mark `stale`
  and wait for Claude Code to refresh it.
- Fixture: `daemon/tests/fixtures/claude_oauth_usage.json` (synthetic values, real shape).

### Session logs — `~/.claude/projects/<project>/**/*.jsonl`
- Includes nested `…/<session>/subagents/agent-*.jsonl` → walk recursively.
- Relevant lines: `type == "assistant"`; fields `timestamp`, `sessionId`, `requestId`,
  `isSidechain`, `message.{id, model, usage}`.
- `usage`: `input_tokens`, `output_tokens`, `cache_read_input_tokens`,
  `cache_creation_input_tokens`, `cache_creation.{ephemeral_5m_input_tokens, ephemeral_1h_input_tokens}`.
- **Dedup required:** one line per content block (826 lines → 349 unique in one file).
  Key = `(message.id, requestId)`. Usage can differ between duplicates (streaming partials:
  `output_tokens` 4 vs 168) → keep the **max/last** per key.
- 1h vs 5m cache writes are priced differently → keep them separate for cost.
- Current volume: 20 files, 21 MB → full rescan is cheap, but tail incrementally anyway
  (byte offset per file + `notify` watcher); prune entries older than 30 d from memory.

## Phases

0. **Setup** ✅ — rustup (1.99.0), `mutter-devkit` for `gnome-shell --devkit`; `just` optional.
1. **Validate sources** ✅ — see findings above.
2. **Daemon MVP** ✅ (2026-10-11) — 10 unit tests; `agent-bar-daemon --print` dumps one snapshot.
   - `model.rs`, `Provider` trait, Claude limits + logs, pricing table, range aggregation.
   - Poll limits every 3 min (backoff on 429/5xx, 10 s timeout); logs event-driven.
   - zbus server; emit `Changed` only on content change.
   - Tests: fixture parsing, dedup, range boundaries (fixed clock), pricing.
   - Milestone: `busctl --user call dev.agentbar.Usage /dev/agentbar/Usage dev.agentbar.Usage GetSnapshot`.
3. **Extension MVP** ✅ (2026-10-11) — verified by `scripts/headless-test.sh` (isolated headless Shell 50,
   screenshots, daemon stop/start, disable/enable, shutdown; fails on any JS error).
   `PanelMenu.Button` (icon + label of the most-constrained active limit),
   dropdown sections: limits (bars + countdown), range switcher 1d/7d/30d (tokens + cost +
   by-model), breakdown, footer (Refresh, “updated Xm ago”). Daemon-absent state. Clean `disable()`.
4. **Packaging** ✅ (2026-10-11) — `just install` / `uninstall` / `status` / `logs`; sandboxed
   `Type=dbus` user unit under graphical-session.target (~17 MB RSS). Was: systemd user unit, `just install` (cargo install, schemas, extension copy),
   optional D-Bus activation.
5. **Polish** ✅ partly (2026-10-11) — Adw prefs (left/used, countdown, top-bar limit,
   notification thresholds), low-quota notifications (once per window), Settings menu item.
   Remaining: poll interval / pricing overrides (daemon config), 24 h sparkline. Was: Adw prefs (panel label choice, poll interval, compact mode, hidden providers),
   threshold notifications (once per window per reset), 24 h sparkline.
6. **Codex provider** — `~/.codex/sessions/**` `token_count` events (rate limits + tokens).

## Dev notes
- `just test` = cargo tests + headless Shell smoke test. `just devkit` = nested Shell window
  with the daemon on the same private bus. `just link-extension` = symlink into
  `~/.local/share/gnome-shell/extensions` (Wayland: log out/in once to discover it).
- This machine's shell env has Nix `GI_TYPELIB_PATH`/`GIO_EXTRA_MODULES`; they crash the
  system gnome-shell, so the scripts unset them.

## Open items
- Pricing table in `pricing.rs` is from the official docs as of 2026-10-06; refresh it when
  models launch (unknown models fall back to their family's current price). Labelled “est.”.
- Ranges are rolling (last 24 h / 7 d / 30 d), not calendar days.
- Config-file overrides for pricing/intervals: deferred to Phase 5.
- Check that the log-derived 1d/7d/30d totals look plausible against `ccusage`.
