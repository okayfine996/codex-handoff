# ADR-001: Centralize app-server sessions

Status: Accepted

## Context

Authentication preflight and quota reads both launch `codex app-server` with
sensitive credentials. Their duplicated process and JSON-RPC handling could
drift, leak child processes, or extend a timeout indefinitely when unrelated
notifications arrive.

## Decision

Use one internal app-server session module to own the private temporary home,
0600 auth file, initialize handshake, response routing, fixed operation
deadline, and child cleanup. Public probe and usage-reader types remain thin
adapters. Doctor's compatibility check performs only the local initialize
handshake; it does not issue account or quota requests.

## Consequences

Protocol lifecycle changes have one implementation and all exits reap the
child. The 45-second usage/preflight limit is a true total deadline. The module
continues to treat every app-server response as untrusted input and never
includes remote error data that could contain credentials.
