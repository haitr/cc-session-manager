# ccsm

Claude Code session manager TUI: browse, filter, preview, trash and restore sessions under `~/.claude/projects`.
Rust + ratatui / ratatui-cheese / ratatui-braille-bar.

## Install

Download the archive for your platform from [Releases](../../releases) and put `ccsm` on your `PATH`.
The Linux build is fully static (musl): one binary for any distro.

## Build

```
cargo run                        # full-screen terminal UI
cargo run --features standalone  # self-contained egui window
cargo test
```

| key | action |
| --- | --- |
| `↑↓` `j k` `PgUp PgDn` `Home End` | move |
| `←` `→` | switch folder tab (`[all]` first); also while typing a filter |
| `space` / `a` | select row / all shown |
| `enter` | open full transcript over the list (`↑↓` `PgUp PgDn` `Home End` scroll, `esc` close) |
| `/` | filter (see below) |
| `d` | move selection (or cursor row) to trash, with confirm |
| `c` | change owner of selection (or cursor row) to the signed-in account, with confirm |
| `y` | copy `cd <cwd> && claude --resume <id>` |
| `tab` | trash view: `space` / `a` select, `r` restore, `x` delete forever (selection, or cursor row) |
| `t` | next colour theme (dark, light, charm, ocean, sunset; not remembered) |
| `?` | key guide |
| `esc` `q` | back / quit |

## Filter

Comma-separated terms. Terms of the same kind are alternatives; every kind present must hold. Case-insensitive.

| term | matches |
| --- | --- |
| `license` | label, path or id contains it |
| `regex:^fix-\d+` | same fields, as a regular expression |
| `group:nx_plugin`, `group:regex:^ios-` | folder name |
| `older:30D` | idle for at least that long |
| `newer:T12H` | active within that span |

Spans are ISO 8601 durations without the leading `P`: `30D`, `2W`, `1M`, `1Y`, `T12H`, `1DT12H`, `T30M`
(`M` is months before `T`, minutes after; a month is 30 days, a year 365).

`group:nx, group:auron, license, older:7D` = (nx or auron) and license and idle a week. To clean up old sessions:
`/older:30D`, `a`, `d`.

Folder tabs list only folders the filter leaves something in.

## Notes

- Trash is a move into `~/.claude/.session-cleaner-trash`; nothing is deleted until `x` in the trash view.
- Sessions owned by a running Claude Code process, or written in the last 10 minutes, are marked `●` and cannot be
  trashed or re-owned.
- Sessions owned by another account (their `session-owner` records vs `oauthAccount` in `.claude.json`) use the theme's second accent colour.
- Session metadata is cached in `~/.claude/ccsm-cache.json` (validated by file size + mtime), so only new or changed
  transcripts are re-read on launch. Safe to delete.

## Releasing

- Bump `version` in `Cargo.toml` and push to `master`: `release.yml` builds Windows / macOS / Linux and publishes `v<version>`.
- Actions → release → Run workflow: publishes a numbered pre-release `v<version>+<run number>` (`ccsm --version` shows it).
- `publish.yml` runs after a proper release to feed package channels; none are configured yet.
