# ch — Codex Handoff

`ch` is a macOS command-line tool for keeping multiple authorized Codex
ChatGPT accounts in a local private vault. It safely switches the live Codex
`auth.json` without reserializing credentials: tokens are never printed,
logged, or uploaded by `ch`.

## Quick start

Requirements: macOS, Codex CLI `0.150.1` with file-based ChatGPT
authentication, and Rust for a source installation.

```sh
git clone <your-repository-url>
cd codex-handoff
cargo install --path .

# Save the account currently signed into Codex.
ch init

# Sign into another authorized account through the official flow.
ch add

# Review every saved account and its current quota.
ch list

# Safely move the live Codex session to a profile.
ch switch work
```

`init` and `add` derive profile names from the local part of the authenticated
email address. For example, `litesky+codex@example.com` becomes
`litesky-codex`.

## Commands

| Command | Description |
| --- | --- |
| `ch init` | Save the existing live `auth.json` as the first profile. |
| `ch add [-f]` | Run official `codex login`, save the new profile, then restore the previously live account. |
| `ch relogin <name> [-f]` | Replace one saved profile using the official login flow. |
| `ch list` | Show all profiles, local health, and quota dashboards. |
| `ch status` | Show the active live profile, its quota, and configured paths. |
| `ch switch <name> [--force / --close-clients]` | Verify a profile online, sync the current one, then atomically activate it. |
| `ch sync [-f]` | Save the latest live token refresh back to the active profile. |
| `ch doctor` | Check the Codex CLI, vault consistency, permissions, locks, and running clients. |

`-f` / `--force` bypasses only the running-client check. It never bypasses
file validation or online authentication verification. `-c` /
`--close-clients` is available on `switch`; it asks Codex and ChatGPT to quit,
then waits up to five seconds. `-f` and `-c` cannot be combined.

## Quota dashboard

`ch list` queries each locally healthy profile in an isolated temporary
`CODEX_HOME`, then displays its 5-hour and weekly windows as progress bars.
Green means less than 50% used, yellow 50–79%, and red 80% or more. The output
also shows reset times, reached limits, spend-control warnings, and available
reset credits.

`ch status` uses the same dashboard for only the active live account. A quota
query does not write either the live or vault authentication file. If one
account's remote query fails, `ch` displays `Usage unavailable` for that
profile without affecting the other accounts or any switching operation.

Color is enabled only in an interactive terminal. Piped or redirected output,
and environments with `NO_COLOR` set, remain plain text with no ANSI escape
codes.

## Paths and configuration

By default, Codex reads live credentials from `~/.codex/auth.json` and `ch`
stores profiles in `~/.codex-handoff`.

| Variable / option | Purpose |
| --- | --- |
| `CODEX_HOME` / `--codex-home` | Override the live Codex home. |
| `CODEX_HANDOFF_HOME` / `--handoff-home` | Override the private profile vault. |
| `CODEX_HANDOFF_CODEX_BIN` / `--codex-bin` | Use a specific `codex` executable. |

Example for a disposable test environment:

```sh
CODEX_HOME=/tmp/codex CODEX_HANDOFF_HOME=/tmp/codex-handoff ch status
```

## Safety model

- Vault and profile directories use `0700`; auth, metadata, state, and lock
  files use `0600`.
- Profile names are validated to prevent path traversal.
- Before `switch`, `ch` starts `codex app-server --stdio` in a temporary home,
  refreshes and verifies the target account, and rejects email mismatches.
- Updates use same-directory temporary files, `fsync`, atomic rename, rollback
  snapshots, and an OS advisory lock.
- `add` and `relogin` never call `logout`; they temporarily preserve and then
  restore the active account.

This first release deliberately does not delete profiles, rotate accounts,
keep tokens alive, synchronize vaults between machines, or restart Codex
Desktop. Use only accounts you are authorized to access and keep normal
machine backups.

## Development

```sh
cargo test
cargo clippy -- -D warnings
cargo audit --no-fetch --stale
```
