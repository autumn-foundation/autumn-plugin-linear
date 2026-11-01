# ADR 0001: Own HMAC check, signed replay window, stateless OAuth

- Status: accepted
- Date: 2026-11-01

## Context

Autumn 0.9 `SignedWebhook` has presets for Stripe, GitHub, Slack, and a
generic `sha256=`-prefixed hex digest. Linear sends a bare hex digest in
`Linear-Signature` and puts the send time in the signed body field
`webhookTimestamp`. Linear asks receivers to refuse a time more than 60 s
from now.

OAuth needs a state nonce and, with PKCE, a code verifier. A server-side
store for them breaks across replicas unless it is shared.

## Decision

1. The plugin owns its HMAC code (`verify`). The check runs on the raw
   bytes, compares in constant time, and tries the current and the
   previous secret.
2. After the HMAC passes, the plugin reads `webhookTimestamp` from the
   signed body. `policy::timestamp_fresh` decides. The header
   `Linear-Timestamp` is not signed, so the plugin does not use it.
3. The nonce lives in a cookie:
   `__Host-linear_oauth_state={nonce}; Path=/; HttpOnly; Secure;
   SameSite=Lax; Max-Age=600`. The `state` parameter is the nonce.
4. The PKCE verifier is `hex(HMAC-SHA256(client_secret, "pkce:" + nonce))`
   (64 chars, in the RFC 7636 set). The callback derives it again. No
   store holds it, and the browser never sees it.

## Consequences

- No wait for an Autumn preset.
- OAuth works across replicas with no shared store.
- A stolen cookie alone does not give the verifier: it needs the client
  secret.

## Alternatives

- Autumn `generic` preset: rejected. It wants a prefix and has no body
  timestamp check.
- A random verifier in the cookie: rejected. The browser would hold it.
- A server-side state store: rejected. It needs a shared store.
