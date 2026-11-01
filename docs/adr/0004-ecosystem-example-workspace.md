# ADR 0004: The ecosystem example is a separate workspace

- Status: accepted
- Date: 2026-11-01

## Context

`examples/intake-desk` uses autumn-harvest, harvest-camel, camel, and San
Mateo. harvest-camel needs `autumn-harvest-plugin`, which turns on the
`autumn-web` default features (libpq, SQLite). The plugin itself uses
`autumn-web` with no default features. In one workspace, Cargo unifies the
features, and every plugin build would pull the C libraries. harvest-camel
also needs Rust 1.89; the plugin MSRV is 1.88.

## Decision

`examples/intake-desk` is its own workspace with its own `Cargo.lock`. It
uses the plugin by path. CI runs it in its own job with Postgres.

## Consequences

- The plugin build stays small and keeps MSRV 1.88.
- Root `cargo` commands do not see the example. CLAUDE.md lists its
  commands.
