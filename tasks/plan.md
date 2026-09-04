# Implementation Plan: Codex Handoff hardening and product roadmap

## Overview

Complete the approved roadmap in dependency order: restore quality gates, deepen the app-server/runtime/inventory modules, improve CLI diagnostics and machine output, add safe profile lifecycle operations, bound and parallelize remote work, then add quota-aware profile selection.

## Decisions

- Keep the vault schema at version 1 and preserve existing profile data.
- Use per-query `--json` output with a top-level `schema_version` field.
- Reject rename/removal for busy profiles and removal for the active profile.
- Run batch remote work with at most four jobs by default.
- Recommend by five-hour usage first and weekly usage second; recommendation may launch an isolated session but never switches the active profile.

## Task List

1. Restore formatting and strict Clippy gates.
2. Deepen app-server session, activity lease, and profile inventory modules.
3. Add CLI help, structured doctor results, protocol compatibility probing, and JSON query output.
4. Add custom aliases, rename, and confirmed removal.
5. Add offline listing, bounded concurrency, and per-profile deadlines.
6. Add quota-aware recommendation and isolated launch.
7. Update documentation and run final review.

## Verification

Every task uses focused tests followed by `cargo test`, `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings`, `cargo audit --no-fetch --stale`, and `git diff --check` as applicable.

