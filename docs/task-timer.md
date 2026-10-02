# Task timer

This fork supports a one-shot action at a specified wall-clock time. To enable
Fast mode at 00:30 China time on October 4, 2026, add this table to
`~/.codex/config.toml` before starting the fork's CLI:

```toml
weekly_quota_reserve_percent = 10

[task_timer]
at = "2026-10-04T00:30:00+08:00"
action = "fast"
```

Replace the example date and time with your deadline. `at` must be a quoted
RFC 3339 timestamp with a timezone: `+08:00` is China time and `Z` is UTC.
Bare clock times, timestamps without a timezone, and invalid actions are
rejected during configuration loading. To interrupt the current task at that
time, use `action = "stop"`. Use `action = "switch"` with `model`,
`reasoning_effort`, or both to change model settings.

The command-line override also works:

```sh
codex -c 'task_timer={at="2026-10-04T00:30:00+08:00",action="fast"}'
```

Omit the table to disable the timer. Restart the CLI after changing its schedule.
It can also be set in a selected profile's configuration file. This is an
absolute, one-shot deadline; it does not repeat daily or count down from startup.
An expired deadline fires as soon as the session loop starts. Each root session
has its own timer; internal review, compaction, memory, and spawned-agent sessions
do not independently fire an inherited schedule.

## Fast mode

`action = "fast"` enables the same Fast mode service tier as the CLI's Fast
selection (`service_tier = "priority"`). It preserves the model and reasoning
effort. It updates thread settings and the running turn's next-step settings,
so later model requests in a long-running task or goal use Fast mode. Requests
and tool actions that already captured their settings retain those settings;
an in-flight request is not restarted. The TUI receives the regular settings
notification and updates its displayed tier. New turns inherit the new tier.
Existing spawned agents inherit the root tier through their normal next-step
propagation.

Fast mode must be enabled by the `fast_mode` feature and advertised by the model
catalog for the selected models. If either the future-turn model or the running
model does not support it, the timer emits a warning and leaves the settings
unchanged. The timer does not require the experimental `step_model_switching`
feature. It changes only the service tier and rechecks managed constraints.

The timer fires while idle as well. After it fires, you can manually change the
tier again; the timer will not override that change. It does not write a global
`service_tier` setting to `config.toml`.

The [weekly quota guard](./weekly-quota-guard.md) remains in force for Fast
requests. Fast mode does not waive the reserve or authorize credit spending.

## Model and reasoning effort

To switch the root task to a selected model and reasoning effort at the deadline:

```toml
[features]
step_model_switching = true

[task_timer]
at = "2026-10-04T00:30:00+08:00"
action = "switch"
model = "gpt-5.5"
reasoning_effort = "low"
```

Choose a model and effort advertised by your model catalog. Examples of effort
names include `low`, `medium`, `high`, and `xhigh`; supported values depend on the
target model. A model-defined effort name is also accepted when advertised.

Omit `model` to change only the reasoning effort. Omit `reasoning_effort` to
change only the model and preserve the selected effort. If a preserved effort
is unsupported by the target model, supply a compatible effort explicitly.
At least one field is required for `switch`. These two fields are rejected with
`fast` and `stop` to prevent silently ignoring a target.

A successful switch updates both the running task's next-step settings and the
thread defaults inherited by later turns. Captured steps and in-flight requests
keep their original settings. Reasoning changes use the normal cache-preserving
effort update mechanism. The TUI receives the model and effort settings update.
Other preferences, including the requested service tier, remain selected and
are resolved normally against the destination model. Already spawned agents
keep their own model and reasoning selections.

For a running task, the experimental `features.step_model_switching` feature
must be enabled. The existing live-switch compatibility and managed-policy
checks still apply, including the admitted approval and Guardian authority.
An idle session can update its future-turn defaults without that feature.
Unknown model metadata, unsupported reasoning effort, disabled live switching,
incompatible authority, and a task or settings replacement during lookup reject
the whole switch and emit a warning. Neither active settings nor future-turn
defaults are changed on rejection. The timer does not restart the task or goal.
Its successful settings commit preserves automatic goal continuation.

The weekly quota guard checks later requests against the newly selected model
as usual. A settings switch itself does not initiate a generation request or
bypass the reserve. The switch is one-shot and does not change global model or
reasoning settings in `config.toml`.

## Stop

`action = "stop"` uses the CLI's normal task interruption lifecycle, cancelling
the current task and stopping its automatic goal continuation. It preserves the
conversation and leaves the session open for manual input. If the session is
idle when the timer fires, there is no current task to interrupt. It does not
block later manual requests or stop unrelated background terminals or external
services. The weekly quota guard separately controls request admission.

The timer uses the system wall clock and rechecks it at least once a second
while waiting, including after system sleep or clock changes. It is owned by
the session submission loop and has no detached task. Session shutdown cancels
the wait. A running submission handler or host scheduling can delay delivery;
the timer is not a real-time scheduling guarantee.

## Verification

```sh
cd codex-rs
cargo test -p codex-core --lib task_timer
cargo test -p codex-core --lib weekly_quota
cargo run -p codex-config-schema --bin codex-write-config-schema
cargo check -p codex-cli --bin codex
```

Tests cover timestamp and target validation, timezone conversion, Fast feature
and model support, model-only and effort-only switches, combined switches,
settings captured before and after a switch, subsequent turns, rejected
switches without partial updates, task replacement during metadata lookup,
normal task interruption, one-shot idle firing, expired deadlines, shutdown,
and inherited timers in subagents. They run locally without consuming model usage.
See [Weekly quota guard](./weekly-quota-guard.md#build-and-run) to build the fork.
