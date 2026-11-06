# Changelog

## [Unreleased]

## [0.2.0] - 2026-11-06

### Changed

- **Breaking:** Support `autumn-web` 0.10. The plugin now needs `autumn-web`
  0.10 and Rust 1.97. Apps on `autumn-web` 0.9 stay on 0.1.0.
- The `intake-desk` example moves to autumn-harvest 0.10 and harvest-camel
  0.3.

## [0.1.0] - 2026-11-02

### Added

- `LinearPlugin`: signed webhooks at `POST /linear/webhooks`, routed by
  `(type, action)`, with a signed replay window, a duplicate guard on
  `Linear-Delivery`, and token cleanup on `OAuthApp` `revoked`.
- Typed views: `IssueData`, `CommentData`, `AgentSessionEvent`;
  `updatedFrom` and label helpers; `entity_key()`.
- OAuth install with PKCE S256, `actor`, and a stateless state cookie;
  refresh with rotation, one refresh per workspace.
- `LinearClient` with rate-limit retries; transport-free `graphql::ops`
  and `parse_response`; `stable_uuid` for idempotent writes.
- `Linear` extractor, health indicator, `testing::EventFixture`.
- A Verus spec of the policy core with a body-drift test.
- Examples: `triage_app` and the `intake-desk` ecosystem example.
