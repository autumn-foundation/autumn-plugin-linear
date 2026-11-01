# ADR 0003: Transport-free GraphQL operations

- Status: accepted
- Date: 2026-11-01

## Context

In the autumn ecosystem, outbound calls can go through `reqwest`, through
the San Mateo proxy (which holds and refreshes tokens), or through a camel
route. Each path needs the same queries and the same error and rate-limit
rules.

## Decision

`graphql` holds the operations and the parser, with no IO:

- `ops::*` returns an `Operation` (`query`, `variables`, `name`). It
  serializes to the GraphQL request body.
- `parse_response(status, retry_after, body)` returns typed data, an
  `Error::GraphQl`, an `Error::LinearApi`, or `Error::RateLimited` with
  the wait.

`LinearClient` is one carrier over `reqwest`. The `intake-desk` example is
another: camel and San Mateo carry the same `Operation`.

## Consequences

- One place for query text, field names, and error rules.
- A new carrier does not copy parsing code.
