# ADR-002: Guard profile lifecycle with activity leases

Status: Accepted

## Context

Isolated Codex runs may read and refresh a profile while management commands
rename, remove, switch, or query it. File existence checks alone cannot prevent
those races.

## Decision

Represent runtime locks as typed RAII activity leases. Isolated runs, usage
reads, and prompts hold compatible shared leases. Inactive-profile lifecycle
mutations require an exclusive lease. The active profile uses the global Codex
home and command-specific client/process guards instead of a profile activity
lease.
Rename also holds the global vault lock and updates profile metadata and active
state together, rolling back on failure. Removal is allowed only for an
existing, non-active profile with an exclusive lease, and the CLI requires an
interactive confirmation or `--yes`.

Profile inventory scanning is centralized, validates directory names, checks
local metadata/auth consistency, rejects symbolic links for sensitive paths,
and always sorts by profile name.

## Consequences

Concurrent readers and Codex runs share one authoritative home, while lifecycle
mutations fail closed instead of racing them. Active usage and prompt operations
may run alongside the default client as described in ADR-004. Removal is
deliberately irreversible, while rename preserves auth bytes and the vault
schema remains at version 1.
