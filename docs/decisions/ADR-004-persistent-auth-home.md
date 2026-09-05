# ADR-004: Run profile-bound Codex operations in the authoritative home

Status: Accepted

## Context

Quota, preflight, and prompt commands previously copied `auth.json` into a
temporary `CODEX_HOME` and copied it back afterward. Codex may rotate OAuth
refresh tokens while any of those commands run. A second Codex or ChatGPT
process can also refresh the active account concurrently, making a copied token
stale and causing otherwise healthy `list` or `status` queries to report usage
as unavailable.

## Decision

Resolve one authoritative home for every existing profile-bound Codex process:
the global Codex home for the active profile, and the vault profile directory
for an inactive profile. Run usage reads, preflight, prompts, isolated Codex
runs, and relogin directly in that home. Usage reads do not proactively request
an authentication refresh. After an active operation, read the final global
auth and transactionally mirror any change into the vault; inactive operations
already update their authoritative auth in place.

Inactive runs, usage reads, and prompts use compatible shared activity leases;
only lifecycle operations that move, replace, rename, or remove a profile need
an exclusive lease. Active usage reads and `hi` prompts may share the global
home with Codex or ChatGPT, while account switching keeps its process guard. A
failed remote operation does not roll a token rotation back. `add` is the
exception because the account has no profile yet: it uses a private staging
home and saves the validated result as a new profile. Doctor also keeps an
authentication-free temporary home for protocol compatibility checks.

## Consequences

Token rotation is committed by Codex to the same file that owns the account, so
`ch` no longer has a copy-back race between temporary and persistent homes.
Usage and prompts are available while a profile is running, subject only to
Codex's own concurrent file behavior. Same-directory temporary files remain in
use for atomic writes and relogin rollback, but never as the authentication home
of an existing profile.
