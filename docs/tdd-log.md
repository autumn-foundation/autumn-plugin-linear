# TDD log

Each batch: spec (where it applies), then tests with stub bodies (RED),
then the code (GREEN), then refactor with the tests green.

| Batch | Modules | RED | GREEN |
|---|---|---|---|
| 1 | `policy` (+ `verus/policy.rs`), `verify` | 17 failed, 19 passed (`error`, `types` hold no logic to stub) | 36 passed, `verus_bodies_match` included |
| 2 | `event`, `config`, `delivery`, `tokens` | 19 failed, 54 passed | 73 passed |
| 3a | `graphql` | 10 failed, 77 passed | 87 passed; refactor: explicit success check (+1 test) |
| 3b | `client` | 9 failed, 91 passed | 100 passed (one test fixture fixed: it lacked `Retry-After`; +1 test for that case) |
| 4 | `webhooks`, `oauth`, `plugin`, `health`, `testing` | 33 failed, 132 passed (the six core flows stubbed; `an_in_flight_duplicate_is_409` skipped: it waits for a handler the stub never starts) | 166 passed |
