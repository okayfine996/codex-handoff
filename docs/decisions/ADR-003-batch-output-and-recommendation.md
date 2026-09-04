# ADR-003: Bound batch work and keep recommendations side-effect free

Status: Accepted

## Context

Sequential profile queries make one slow account delay every result. Automation
also needs output that does not depend on terminal rendering. A quota-based
selector must not silently change the desktop account.

## Decision

Run list and hi work with four workers by default and accept 1–16. Each usage
session has a fixed 45-second deadline; each hi process defaults to 120 seconds
and is killed at its deadline. Preserve per-profile failures and sort final
results by name.

JSON query documents use `schema_version: 1`. Recommendation considers healthy
profiles with available quota and no reached limit, preferring `codex`, then
`default`, then a sole bucket. It ranks by primary used percentage, secondary
used percentage (missing last), earliest primary reset, then name.

`best` only reports a profile. `run best` may launch that profile in isolation,
but recommendation never switches the active profile.

## Consequences

Batch latency is bounded without making output nondeterministic. Scripts can
distinguish no eligible profile via exit status 2 and failed doctor checks via
exit status 1. An actual profile named `best` is reserved by the `run` command's
selector syntax and can still be managed by the other profile commands.
