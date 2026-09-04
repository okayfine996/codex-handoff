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

# Work offline or ask which healthy profile has the most quota remaining.
ch list --offline
ch best

# Safely move the live Codex session to a profile.
ch switch work

# Start an isolated Codex CLI session without changing the live account.
ch run work
```

`init` and `add` derive profile names from the local part of the authenticated
email address. For example, `litesky+codex@example.com` becomes
`litesky-codex`.

## Commands

| Command | Description |
| --- | --- |
| `ch init` | Save the existing live `auth.json` as the first profile. |
| `ch add [--name <alias>] [-f]` | Run official `codex login`, optionally save under an alias, then restore the previously live account. |
| `ch relogin <name> [-f]` | Replace one saved profile using the official login flow. |
| `ch rename <current> <new-name>` | Rename an idle profile and update its metadata. |
| `ch remove <name> [--yes]` | Permanently remove a non-active, idle profile after confirmation. |
| `ch list [--offline] [--concurrency 1..16]` | Show all profiles; offline mode performs local checks only. |
| `ch status` | Show the active live profile, its quota, and configured paths. |
| `ch switch <name> [--force / --close-clients]` | Verify a profile online, sync the current one, then atomically activate it. |
| `ch run <name> [-- <codex args>...]` | Start Codex with the selected profile; the active profile uses the default `CODEX_HOME`, while other profiles use their persistent profile home. |
| `ch sync [-f]` | Save the latest live token refresh back to the active profile. |
| `ch best [--concurrency 1..16]` | Recommend an eligible profile by current quota without switching accounts. |
| `ch hi [prompt] [--concurrency 1..16] [--timeout <seconds>]` | Send a bounded prompt ("hi" by default) across all saved accounts. |
| `ch doctor` | Check the Codex CLI, local app-server handshake, vault consistency, permissions, locks, and running clients. |

`-f` / `--force` bypasses only the running-client check. It never bypasses
file validation or online authentication verification. `-c` /
`--close-clients` is available on `switch`; it asks only the default
Codex/ChatGPT lane to quit, then waits up to five seconds. A `ch run` session
for the target profile must be ended manually. `-f` and `-c` cannot be combined.

`remove` is intentionally stricter: the active profile and any profile with a
running isolated session cannot be deleted. Non-interactive callers must pass
`--yes`; removal is permanent and does not affect the live account.

## Parallel CLI sessions

Use `ch run` when you need multiple authorized accounts at once. A non-active
profile leaves the default `~/.codex/auth.json` untouched, so `switch` remains
the command for changing the account used by Codex Desktop, a direct `codex`
invocation, and existing scripts.

```sh
# Separate terminals can use separate profiles at the same time.
ch run personal
ch run work -- -C ~/src/client --no-alt-screen
```

For the active profile, `ch run` deliberately uses the normal default
`CODEX_HOME`; it behaves exactly like launching `codex` directly and can share
the account with Codex Desktop. For every non-active profile, the saved profile
directory is its persistent `CODEX_HOME`. On the first such `run`, `ch` copies
`config.toml` and `*.config.toml` from the default Codex home when present;
later changes are profile-specific. It never copies authentication, session
history, or plugin installations.

Native Codex supports multiple instances of one account, so active-profile
sessions are not artificially serialized. Non-active sessions for the same
profile also run concurrently, but `ch` holds a shared runtime lock while they
run: `list` and `hi` report that profile as busy instead of refreshing its auth
file concurrently.

`switch` remains necessary because Codex Desktop and a plain `codex` invocation
always use the default home. It blocks only the default lane (the current active
account) and any `ch run` session for the target profile; unrelated profile
sessions keep running. `--close-clients` closes only the default lane.

## Quota dashboard

`ch list` queries each locally healthy profile in an isolated temporary
`CODEX_HOME`, then displays its 5-hour and weekly windows as progress bars.
Green means less than 50% used, yellow 50–79%, and red 80% or more. The output
also shows reset times, reached limits, spend-control warnings, and available
reset credits.

`ch status` uses the same dashboard for only the active live account. When a quota
query or `hi` prompt triggers a credential refresh, `ch` automatically persists the
updated tokens back to the vault profile (and the live `auth.json` for the active
account) to keep OAuth refresh token rotations in sync. To avoid racing an active
client, `status`, `list`, and `hi` do not run a temporary refresh for the default
account while Codex or ChatGPT is running; `list` and `hi` similarly skip a
non-active profile that is currently running through `ch`. If one account's remote
query fails, `ch` displays `Usage unavailable` for that profile without affecting
the other accounts or any switching operation.

`list` and `hi` run at most four profile operations concurrently by default.
Use `--concurrency` to select 1–16 workers. Usage app-server sessions have a
fixed 45-second deadline; `hi` has a 120-second per-profile deadline by default
and supports `--timeout`. Results remain sorted by profile name regardless of
completion order.

`ch best` considers only locally healthy profiles with an available primary
quota window and no reached quota or spend-control limit. It prefers the
`codex` bucket, then `default`, then a sole bucket, and ranks by primary usage,
secondary usage, earliest primary reset, and profile name. A missing secondary
window ranks after a present one. `ch run best` launches the recommendation in
the same isolated mode as `ch run <name>` and never changes the active profile.

Color is enabled only in an interactive terminal. Piped or redirected output,
and environments with `NO_COLOR` set, remain plain text with no ANSI escape
codes.

## Machine-readable output

Pass the global `--json` option to `list`, `status`, `doctor`, or `best` for a
stable JSON document with `schema_version: 1`.

```sh
ch --json list --offline
ch --json doctor
ch --json best
```

Partial remote failures stay in the document instead of hiding other profiles.
`best` exits with status 2 when no profile is eligible; `doctor` exits with
status 1 when a check fails.

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
- Sensitive profile files and directories reject symbolic links instead of
  following them.
- Before `switch`, `ch` starts `codex app-server --stdio` in a temporary home,
  refreshes and verifies the target account, and rejects email mismatches.
- Updates use same-directory temporary files, `fsync`, atomic rename, rollback
  snapshots, and an OS advisory lock.
- `add` and `relogin` never call `logout`; they temporarily preserve and then
  restore the active account.

If `doctor` reports insecure credentials, remove group/other permissions from
the affected file before retrying. For the default paths:

```sh
chmod 600 ~/.codex/auth.json
chmod 700 ~/.codex-handoff ~/.codex-handoff/profiles ~/.codex-handoff/profiles/*
```

This release deliberately does not automatically rotate accounts, switch as a
side effect of recommendation, keep tokens alive, synchronize vaults between
machines, or restart Codex Desktop. Use only accounts you are authorized to
access and keep normal machine backups.

## Development

```sh
cargo test
cargo clippy -- -D warnings
cargo audit --no-fetch --stale
```
