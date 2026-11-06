# autumn-plugin-linear

Build Linear apps on [Autumn](https://autumn-web.app). The plugin adds:

- **Signed webhooks**, routed by type and action, with a replay window and
  a duplicate guard.
- **OAuth install** (PKCE, `actor=app`) with token refresh per workspace.
- **A GraphQL client** that retries rate-limited calls, and
  **transport-free operations** that any carrier can send (San Mateo, camel).

## Quickstart

```toml
# Cargo.toml
[dependencies]
autumn-plugin-linear = { git = "https://github.com/autumn-foundation/autumn-plugin-linear" }
autumn-web = "0.10"
tracing = "0.1"
```

```sh
export LINEAR_WEBHOOK_SECRET="<webhook signing secret>"
export LINEAR_API_KEY="lin_api_..."        # one workspace
```

```rust
use autumn_plugin_linear::{LinearPlugin, WebhookContext};
use autumn_web::prelude::*;

async fn issue_created(ctx: WebhookContext) -> AutumnResult<()> {
    let issue = ctx.event.issue().map_err(|e| e.to_autumn_error())?;
    tracing::info!(org = %ctx.event.organization_id, issue = ?issue.map(|i| i.id), "issue created");
    Ok(())
}

#[autumn_web::main]
async fn main() {
    autumn_web::app()
        .plugin(LinearPlugin::new().on_action("Issue", "create", issue_created))
        .run()
        .await;
}
```

In Linear, open Settings → API, add a webhook to
`https://<host>/linear/webhooks`, and copy its signing secret.

## What the plugin adds

| Piece | Detail |
|---|---|
| Config section | `[linear]`. Secrets come only from the environment. |
| `POST /linear/webhooks` | Checks the HMAC (401), the body size (413), the envelope and `Linear-Delivery` (400), and the signed `webhookTimestamp` (401). Skips done duplicates (200). Answers 409 while the same delivery runs. Deletes the token on `OAuthApp` `revoked`. Runs the handler. |
| `GET /linear/auth` | Starts the install: state cookie, PKCE, redirect to Linear. |
| `GET /linear/auth/callback` | Checks the state, exchanges the code, reads the workspace, stores the token, runs the install handler. |
| `Linear` extractor | `client()` (API key), `client_for(&org)` (workspace token). `Linear::install_url()` gives the install path. |
| Health | A `"linear"` indicator in API-key mode, in `/actuator/health` only. |

The three routes are `#[public]`, so `autumn routes audit` accepts them.
OAuth routes answer 404 when `app_url` is not set. The webhook route
answers 404 when `LINEAR_WEBHOOK_SECRET` is not set.

## Handlers

```rust
LinearPlugin::new()
    .on_action("Issue", "create", on_new_issue)   // one action
    .on("Comment", on_comment)                     // every action of a type
    .on("AgentSessionEvent", on_agent_session)     // agent sessions
```

The `(type, action)` handler wins over the `type` handler. A malformed or
repeated key stops the startup. Routing uses the signed body `type`, not
the unsigned `Linear-Event` header.

A handler gets a `WebhookContext`:

| Field or method | Gives |
|---|---|
| `event.event_type`, `event.action` | The signed type and action. |
| `event.organization_id` | The workspace. Use it as the tenant key. |
| `event.issue()`, `event.comment()`, `event.agent_session()` | Typed views. `None` for another type. |
| `event.changed(field)`, `event.previous(field)` | `updatedFrom` helpers. |
| `event.label_added(id)`, `event.label_removed(id)` | Label moves. |
| `event.entity_key()` | A key from signed fields, for idempotent work. |
| `delivery_id` | `Linear-Delivery`. Not signed. |
| `client()` | The GraphQL client for the workspace. |
| `state`, `linear` | The app state and the plugin handle. |

For agents, `agent_session()` gives the session id, the issue,
`prompt_context`, `prompt_body()`, and `is_stop()`. On `created`, send an
activity within 10 seconds:

```rust
use autumn_plugin_linear::ActivityContent;

let session = ctx.event.agent_session().map_err(|e| e.to_autumn_error())?;
if let Some(session) = session {
    let client = ctx.client().await.map_err(|e| e.to_autumn_error())?;
    let thought = ActivityContent::Thought { body: "On it.".into() };
    client
        .create_agent_activity(session.session_id(), &thought)
        .await
        .map_err(|e| e.to_autumn_error())?;
}
```

## Configuration

| Key | Environment variable | Default | Meaning |
|---|---|---|---|
| `client_id` | `LINEAR_CLIENT_ID` | none | OAuth client id. OAuth needs it. |
| `app_url` | `LINEAR_APP_URL` | none | Public base URL. Setting it turns OAuth on. |
| `scopes` | `LINEAR_SCOPES` (comma-separated) | `["read"]` | Scopes OAuth asks for. |
| `actor` | `LINEAR_ACTOR` | `user` | `user` or `app`. Use `app` for agents. |
| `after_install_path` | `LINEAR_AFTER_INSTALL_PATH` | `/` | Local path after an install. Gets `organization=`. |
| `prompt_consent` | `LINEAR_PROMPT_CONSENT` | `true` | Send `prompt=consent`, so a user can install in a second workspace. |
| `webhook_tolerance_secs` | `LINEAR_WEBHOOK_TOLERANCE_SECS` | `60` | Replay window (1–600). |
| `max_rate_limit_retries` | `LINEAR_MAX_RATE_LIMIT_RETRIES` | `3` | Retries of a rate-limited call (0–10). |
| `max_rate_limit_wait_secs` | `LINEAR_MAX_RATE_LIMIT_WAIT_SECS` | `10` | Longest wait before one retry (0–300). |

Precedence: `LinearPlugin::configure` > environment > `autumn.toml` >
defaults. `LinearPlugin::env_lookup` replaces the process environment,
for example with a vault lookup.

| Secret | Holds |
|---|---|
| `LINEAR_WEBHOOK_SECRET` | Webhook signing secret. Webhook handlers need it. |
| `LINEAR_WEBHOOK_SECRET_PREVIOUS` | Old signing secret, during rotation. |
| `LINEAR_CLIENT_SECRET` | OAuth client secret. OAuth needs it. |
| `LINEAR_API_KEY` | Personal API key (one workspace). |

A secret key (`secret`, `token`, `api_key`, `password`, `*_secret`,
`*_token`, `*_key`) in `[linear]` stops the startup.

## Two app shapes

**One workspace.** Set `LINEAR_API_KEY`. `client()` and `client_for(_)`
use it while OAuth is off.

**Many workspaces (OAuth app).** Set `app_url`, `client_id`, `scopes`, and
`LINEAR_CLIENT_SECRET`. Set the redirect URL to
`https://<host>/linear/auth/callback`. Users install through
`/linear/auth`. `client_for(&org)` uses the stored token and refreshes it
5 minutes before it expires. On `NotInstalled` or `Reauthorize`, send the
browser to `Linear::install_url()`.

Run code after each install with `.on_install(handler)`. The handler gets
the workspace, the granted scopes, the callback request headers, and a
`Linear` handle. Bind the workspace to your user there: read your session
cookie from `ctx.headers`. Do not bind it on the page after the install.
The `organization` value in that URL is not signed, and anyone can type
it. With `actor = "user"`, each new install by a workspace member
replaces the stored token. Use `actor = "app"` for a bot.

## Calling the API

```rust
use autumn_plugin_linear::graphql::{CommentCreate, IssueUpdate};

let client = linear.client_for(&org).await.map_err(|e| e.to_autumn_error())?;
let issue = client.issue("ENG-123").await.map_err(|e| e.to_autumn_error())?;
client
    .update_issue(&issue.id, &IssueUpdate { priority: Some(2), ..IssueUpdate::default() })
    .await
    .map_err(|e| e.to_autumn_error())?;
let comment = CommentCreate::new(&issue.id, "Triaged.").with_stable_id("triage:ENG-123");
// Posts the comment only when no comment has its id.
client.create_comment_once(&comment).await.map_err(|e| e.to_autumn_error())?;
```

Helpers: `viewer`, `organization`, `issue`, `issues` (one page,
`Connection<Issue>`), `create_issue`, `update_issue`, `create_comment`,
`create_comment_once`, `find_comment`, `create_agent_activity`,
`create_agent_activity_with_id`, `add_agent_session_url`,
`create_webhook`, `delete_webhook`. `query::<T>()` runs your own document.

The client retries only rate-limited calls (429, or `RATELIMITED` with no
`data`). It waits for `Retry-After`, else until
`X-RateLimit-Requests-Reset`, else 1 second. A wait longer than `max_rate_limit_wait_secs`
stops the retries: `Error::RateLimited` keeps the wait, so a job queue can
run the work later. A retry of another failure can run a mutation two
times, so the client does not do it. Give each mutation a client id
(`with_stable_id`) and look it up before you create it again. The client
does not follow redirects.

### Other carriers

`graphql::ops` builds an `Operation` with no IO. Send `op.body()` through
any carrier, then read the answer with `graphql::parse_response` and
`op.extract`. `examples/intake-desk` sends the operations through camel
and the San Mateo proxy, which holds the OAuth token.

## Errors

Call `err.to_autumn_error()` to get the HTTP status in this table. Do not
use `?` to change an `Error` into an `AutumnError`: Autumn's blanket
`From` gives 500. `err.is_transient()` tells a job queue to retry.

| Kind | Status |
|---|---|
| `Signature`, `Replay`, `NotInstalled`, `Reauthorize` | 401 |
| `InvalidRequest`, `Json` | 400 |
| `StateMismatch`, `AuthorizationDenied` | 403 |
| `OAuthDisabled`, `WebhooksDisabled` | 404 |
| `InProgress` | 409 |
| `PayloadTooLarge` | 413 |
| `Http`, `LinearApi`, `GraphQl`, `UnexpectedResponse` | 502 |
| `RateLimited`, `MissingRuntime` | 503 |
| `Config`, `Store` | 500 |

## Production notes

- **Respond fast.** Linear waits 5 seconds and then retries (1 min, 1 h,
  6 h). Hand slow work to a job queue or a workflow engine, and return
  `Ok` only after the hand-off is durable.
- **At least once.** The duplicate guard is a fast path. A retry can
  carry a new `Linear-Delivery` id. Make the work idempotent with your own
  key (for example the issue id) or `event.entity_key()`. See ADR 0002.
- **Unsigned headers.** Linear signs the body only. The plugin reads type,
  action, workspace, and time from the signed body.
- **Bring a store.** `InMemoryTokenStore` and `InMemoryDeliveryStore` lose
  data on a restart and do not share data across replicas. Implement
  `TokenStore` and `DeliveryStore` on your database, and install them with
  `.token_store(...)` and `.delivery_store(...)`. `WorkspaceToken`
  serializes both tokens in clear text: encrypt them at rest.
- **Refresh across replicas.** One process runs one refresh per workspace
  at a time. Linear gives a refresh a 30 minute grace period. When Linear
  refuses a refresh, the plugin uses a fresh pair from another replica. It
  deletes the token only when the store still holds the refused pair.
  Implement `TokenStore::delete_if` as one atomic step in a shared store.
- **Refresh failures.** After a failed refresh, the plugin waits 10
  seconds before the next one. A caller with an expired token gets
  `RefreshBackoff` (503) in that time. A refused client secret
  (`invalid_client`) is a config error: the tokens stay.
- **Revoke.** On `OAuthApp` `revoked`, the plugin deletes the token, unless
  a later install stored it. A revoke for another `oauthClientId` changes
  nothing. When the event has no `createdAt`, the plugin asks Linear
  whether the token still works.
- **One secret, many workspaces.** The plugin checks one signing secret
  (plus the previous one). When you create a webhook in each workspace
  with `create_webhook`, an admin of one workspace can read its secret.
  Give each workspace its own webhook route and secret in that case.
- **CSRF.** When `security.csrf.enabled` is on, add `/linear/webhooks` to
  `security.csrf.exempt_paths`. The plugin logs a warning when the path is
  missing.
- **Your own comments.** A bot that answers `Comment` events also gets its
  own comments. Compare `comment().user_id` with your app user id.
- **Secret rotation.** A new webhook signing secret takes effect at once.
  Keep the old one in `LINEAR_WEBHOOK_SECRET_PREVIOUS` while you move
  webhooks over.

## Testing your handlers

- `testing::EventFixture` builds a body. `.sign(secret, delivery_id)` gives
  the body and the headers that the plugin accepts.
- `LinearConfig::from_sources(toml, lookup)` builds a config without the
  process environment. `LinearPlugin::env_lookup` does the same for a
  running app.
- `LinearPlugin::api_base_url("http://127.0.0.1:<port>")` sends GraphQL and
  token calls to a local stub. The plugin accepts only loopback hosts.
- To call a handler directly, build a runtime, put it in a test state, and
  read the handle:

  ```rust,ignore
  let runtime = LinearPlugin::new().build_runtime(LinearConfig::from_sources(None, lookup)?)?;
  let state = AppState::for_test();
  state.insert_extension(runtime);
  let linear = Linear::from_state(&state)?;
  let ctx = WebhookContext::new(DeliveryId::parse("d-1")?, signed.body, linear)?;
  ```

  `InstallContext::new(Organization::new("org-1"), "read", linear)` works
  the same way.

## Examples

| Example | Shows | Run |
|---|---|---|
| `examples/triage_app.rs` | The plugin alone: a handler that sets a priority and posts one comment. | `cargo run --example triage_app` |
| `examples/intake-desk` | The ecosystem: plugin → camel → harvest workflow (one per issue) → camel → San Mateo → Linear (wiremock, over real HTTP). One module per part; `main.rs` shows the whole flow. Self-checking. Needs Postgres. | `DATABASE_URL=... cargo run --locked --manifest-path examples/intake-desk/Cargo.toml` |

## Crate layout

| Module | Purpose |
|---|---|
| `config` | `[linear]` config, `LINEAR_*` variables, builder closures |
| `types` | `OrganizationId`, `EventType`, `Action`, `DeliveryId`, `Actor` |
| `policy` | The verified core (Verus spec in `verus/policy.rs`) |
| `verify` | Webhook HMAC check |
| `event` | `WebhookEvent` and typed views |
| `webhooks` | The webhook route, extractor, and handler registry |
| `delivery` | Duplicate-delivery guard |
| `oauth` | Install and callback routes, code exchange, refresh |
| `tokens` | Token storage |
| `graphql` | Transport-free operations and the response parser |
| `client` | `LinearClient` over `reqwest` |
| `health` | Health check |
| `plugin` | `LinearPlugin`, `LinearRuntime`, `Linear`, `InstallContext` |
| `testing` | Signed event fixtures |
| `error` | `Error`, `ErrorKind`, HTTP status mapping |

## Not in this release

- Client-credentials tokens.
- Webhook source IP checks.
- Agent session plan updates.
- Autumn profile layering for `[linear]`.

## License

Apache-2.0. See `LICENSE`.
