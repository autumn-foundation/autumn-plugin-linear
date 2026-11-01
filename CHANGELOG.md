# Changelog

## [Unreleased]

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
