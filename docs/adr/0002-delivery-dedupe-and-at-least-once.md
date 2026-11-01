# ADR 0002: Delivery dedupe and at-least-once hand-off

- Status: accepted
- Date: 2026-11-01

## Context

Linear retries a failed delivery up to 3 times (1 min, 1 h, 6 h). It sends
`Linear-Delivery`, a UUID for each payload. The docs do not say that a
retry keeps the same value. The header is not signed. Linear waits 5 s
for a 200.

An app that acts on issues must not lose one and must not act two times
on it.

## Decision

1. The webhook route claims `Linear-Delivery` in a `DeliveryStore` before
   the handler runs. Done gives 200. In flight gives 409. A failure or a
   panic releases the claim, so the next retry runs again.
2. The plugin gives at-least-once delivery to the handler, not exactly
   once. The dedupe is a fast path, not a guarantee.
3. `WebhookEvent::entity_key()` gives a key from signed fields
   (`type`, `action`, `data.id`, `createdAt`). Apps use it, or their own
   work key (for example the issue id), for idempotent work.
4. A handler returns `Ok` only after a durable hand-off (a job queue or a
   workflow engine). The `intake-desk` example hands off to harvest, which
   starts one workflow per issue id.
5. Write-back helpers take a client-chosen id (`create_comment`), so a
   retry of the same work can find its earlier result.

## Consequences

- A retry with a new delivery id runs the handler again. The app key
  makes that safe.
- A replayed body with a forged delivery id must pass the HMAC and the
  60 s window. The app key makes it a no-op.
