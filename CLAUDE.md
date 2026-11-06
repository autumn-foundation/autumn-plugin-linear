# CLAUDE.md — autumn-plugin-linear

## Commands

```sh
export PATH="$HOME/.cargo/bin:$PATH"
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
cargo test --locked --doc
cargo +1.97 clippy --locked --all-targets --all-features -- -D warnings   # MSRV
cargo llvm-cov --summary-only                                            # keep >= 85 % lines
verus verus/policy.rs                                                    # CI job `verus`
# The ecosystem example is its own workspace (ADR 0004). It needs Postgres.
cargo fmt --manifest-path examples/intake-desk/Cargo.toml --check
cargo clippy --locked --manifest-path examples/intake-desk/Cargo.toml --all-targets -- -D warnings
INTAKE_DESK_TEST_DATABASE_URL=postgres://u:p@localhost/db cargo test --locked --manifest-path examples/intake-desk/Cargo.toml
```

All gates pass before a push. `Cargo.lock` is committed (both workspaces).

## Architecture

| Module | Owns | Depends on |
|---|---|---|
| `error` | `Error`, `ErrorKind`, `status()`, `is_transient()`, `to_autumn_error()` | `autumn_web`, `thiserror` |
| `types` | `OrganizationId`, `EventType`, `Action`, `DeliveryId`, `Actor` | `error` |
| `policy` | Pure decisions: replay window, refresh, retry, claim state | none |
| `verify` | `SecretSet`, hex HMAC | `hmac`, `sha2`, `hex` |
| `event` | `WebhookEvent`, typed views | `types` |
| `config` | `LinearConfig`: `[linear]` + `LINEAR_*` | `types`, `verify`, `client`, `policy`, `oauth` |
| `tokens` | `WorkspaceToken`, `TokenStore`, `InMemoryTokenStore` | `policy` |
| `delivery` | `DeliveryStore`, `InMemoryDeliveryStore` | `policy` |
| `graphql` | `Operation`, `ops`, `parse_response`, typed nodes | `error` |
| `client` | `LinearClient`, `Credential`, `RetryPolicy` | `graphql`, `policy`, `reqwest` |
| `webhooks` | Route, `VerifiedWebhook`, `WebhookRegistry`, `process` | `verify`, `event`, `plugin` |
| `oauth` | Install and callback routes, exchange, refresh, PKCE | `plugin`, `client` |
| `health` | `LinearHealth` | `client` |
| `plugin` | `LinearPlugin`, `LinearRuntime`, `Keeper`, `Linear`, `InstallContext`, `mount` | all above |
| `testing` | `EventFixture`, `SignedEvent` | `verify`, `webhooks` |

Webhook flow:

1. Linear `POST`s `/linear/webhooks`.
2. `VerifiedWebhook` checks the HMAC on raw bytes (401).
3. It parses the envelope (400) and checks the signed `webhookTimestamp` (401).
4. It reads `Linear-Delivery` (400).
5. `process` claims the delivery. Done gives 200. In flight gives 409.
6. A spawned task runs the revoke cleanup on `OAuthApp` `revoked`, then the handler.
7. Success calls `complete`. An error or a panic calls `release`.

## Rules

- README Rust snippets are doctests. Make them compile; use `no_run`, not `ignore`.
  `tests/doc_snippets.rs` checks the TOML snippets and the version pins in the docs.
- Secrets travel only through `LINEAR_WEBHOOK_SECRET`,
  `LINEAR_WEBHOOK_SECRET_PREVIOUS`, `LINEAR_CLIENT_SECRET`,
  `LINEAR_API_KEY` (or the `env_lookup` closure). Never in `autumn.toml`,
  code, logs, `Debug`, or error text.
- `unsafe_code = "forbid"`. Tests never call `std::env::set_var`.
- No `unwrap`/`expect`/`panic!`/`todo!` in production code.
- Never `?` an `Error` into `AutumnError`. Call `to_autumn_error()`.
- HMAC checks run on raw bytes, before any parsing, in constant time.
  Route and check time on signed body fields, never on headers.
- Retry only rate-limited calls. A mutation must not run two times.
- Time and randomness come from `AppState::clock()` and
  `AppState::entropy()`.
- `Plugin::build` has no side effects. IO happens in the `on_startup` hook.
- Every token write and delete goes through `Keeper` and its per-workspace
  lock. Never remove an entry from the lock table.
- A change to `src/policy.rs` needs the same change in `verus/policy.rs`.
  `policy::tests::verus_bodies_match` compares the bodies and constants.
- Plugin routes carry `#[public]`. Test the classification.
- Unit tests live in `src/<module>/tests.rs`. Network tests use
  `test_support::Stub`. No test needs Linear.
- `examples/intake-desk` has one module per part (`webhook`, `camel`,
  `harvest`, `workflow`, `triage`, `auth`, `mock_linear`, `scenario`).
  `main.rs` keeps the whole flow in one place: the diagram, the module
  table, and `run`. Its Linear API is wiremock; San Mateo calls it over
  real HTTP.
- Docs and comments in ASD-STE100 style: short sentences, active voice,
  simple present tense. `missing_docs` warns.
- `SUPPORTED_AUTUMN_WEB` in `src/plugin.rs` equals the `autumn-web`
  requirement in `Cargo.toml` (a test checks it).
- Ground Linear facts on linear.app/developers or the SDK schema and
  record them in `docs/planning.md`. Never from memory.
- Write the test first. See it fail. Then write the code. Log each batch
  in `docs/tdd-log.md`.
