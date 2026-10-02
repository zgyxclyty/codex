# Weekly quota guard

This fork can stop new ChatGPT model requests when the weekly included allowance
reaches a configurable reserve. To preserve 10%, add this at the top level of
`~/.codex/config.toml`:

```toml
weekly_quota_reserve_percent = 10
```

Valid values are whole percentages from 0 to 100. Omit the setting to disable the
guard. Setting `0` enables the guard and still stops requests when included usage
is exhausted. Setting `100` stops every request subject to weekly limits. The
standard CLI configuration override also works:

```sh
codex -c weekly_quota_reserve_percent=10
```

Restart the CLI after changing the configuration file. The reserve is captured
when the session's model client is created.

The setting is also accepted in configuration profiles. The guard applies to
ChatGPT authentication, including ChatGPT token authentication. API-key and
third-party provider requests use their existing billing behavior.

## Behavior

Before each HTTP or WebSocket generation request, the client reads the account
usage endpoint with the current request's authentication and configured network
policy. This includes tool continuations, automatic retries, compaction requests,
and background memory summaries. WebSocket prewarm and creation of new realtime
calls are also checked, along with standalone native image-generation/edit and
web-search requests. The guard checks the shared Codex quota and any reported
quota identified with the selected model.

The request stops when weekly remaining usage is **at or below** the reserve.
It also stops if a relevant included usage window (such as the five-hour window)
is exhausted, or the backend reports that ordinary included usage is unavailable.
Available purchased credits, unlimited credit balances, and reset credits do not
bypass the guard. The guard only reads usage; it does not consume reset credits.

The guard fails closed: failed or timed-out lookups, invalid percentages, expired
windows, and missing weekly information stop the request. A lookup has a
10-second timeout. The error explains the reason. When the weekly reserve is
reached, it also shows the configured reserve and reset time when available.
A stopped request is terminal and does
not enter stream retries. An active goal becomes `blocked`, which stops its
automatic continuation. Once usage resets, submit a new request or explicitly
resume the goal. A fresh usage lookup runs on each request.

This is a client-side admission check. An already accepted request can consume
more than the remaining headroom, and another client can spend the allowance
between the check and the request. Choose a reserve large enough for your longest
requests and concurrent usage. The feature takes effect in clients running this
fork; an installed official CLI or desktop app continues using its own binary.
Existing realtime calls are not continuously monitored.
External MCP services and tools with their own billing are outside this guard.

## Build and run

Install the repository's Rust toolchain, then build from the checkout:

```sh
cd codex-rs
cargo build -p codex-cli
./target/debug/codex -c weekly_quota_reserve_percent=10
```

For an optimized executable, run `cargo build --release -p codex-cli` and use
`./target/release/codex`. See [Installing and building](./install.md) for the
repository's other prerequisites and package assembly instructions.

## Verification

Focused tests use local mock servers and dummy authentication; they do not
consume model usage:

```sh
cd codex-rs
cargo test -p codex-core --lib weekly_quota
cargo test -p codex-image-generation-extension -p codex-web-search-extension --lib weekly_quota
cargo test -p codex-goal-extension --test goal_extension_backend turn_error_blocks_goal
cargo run -p codex-config-schema --bin codex-write-config-schema
```

These tests exercise the reserve boundary, exhausted short windows, credit
balances, missing/invalid/expired usage, fresh lookups and reset recovery,
selected-model quotas, API-key bypass, HTTP/WebSocket/prewarm interception,
same-turn continuations, configuration range validation, and goal blocking.
The native image and web-search tests also assert that only usage lookups reach
the mock server after the reserve is reached.
