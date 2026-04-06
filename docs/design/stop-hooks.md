# Stop hooks (astra)

## Behavior

When the agentic loop believes the model is **done**, astra can inject a **user message** that asks the model to run **verification commands via the normal `bash` tool** (permissions, TurnGuard, audit). See `rust/crates/runtime/src/turn/stop_hooks.rs`.

Sources of hooks (merged in order):

1. **Declarative** — **`.astra/stop-hooks.yaml`** or **`.astra/stop-hooks.yml`** under the **project root** (resolved via `git_root` / `cwd` in `EdgeContext`). Matches `rust/crates/runtime/src/turn/stop_hooks_yaml.rs`.
2. **Auto-detect** — optional single `verify-changes` hook with stack hints (Cargo, npm, Go, Python) when `auto_detect` is true (default).

## Declarative file format

```yaml
# .astra/stop-hooks.yaml
version: 1

# When true (default), append the smart "verify-changes" hint after your hooks
# when the turn is classified as mutating (`verification_required`).
auto_detect: true

hooks:
  - label: unit-tests
    command: cargo test -q
    # optional; relative to project root, cannot escape above root
    working_dir: .
    # stop (default) | task_completed | teammate_idle
    when: stop
    enabled: true
```

### Phases (`when`)

| Value | Status |
|-------|--------|
| `stop` | **Wired** — completion prompt on normal turns (not plan subtasks). Hyphens in YAML (`teammate-idle`) normalize to underscores. |
| `task_completed` | **Wired** — same completion mechanism as `stop`, but only when `is_plan_subtask` (durable plan sub-run). Ignores `when: stop` entries for that turn. |
| `teammate_idle` | **Wired** — after a `delegate` tool round returns, a one-time user message asks the model to run these checks via `bash` (same permission path as stop hooks). |

### Read-only turns

If the message is classified as **read-only** (no `verification_required`), **auto-detect is skipped**. **Declarative `stop` hooks still apply** so teams can force an audit even on “explain” turns when needed.

### Cloud / server runs

The server resolves a **local filesystem** project root from the chat request `context` (`EdgeContext`): **`edge_profile.git_root`**, else **`edge_profile.cwd`**. It reads **`<project_root>/.astra/stop-hooks.yaml`** (or `.yml`) on the **machine running the server** (same layout as the CLI). If those fields are missing or the path has no hooks file, hook lists stay empty.

For **delegation sub-runs**, the optional keys **`git_root`**, **`workspace_root`**, and **`cwd`** in the delegation `context` map are checked in that order; when present, the same YAML loading applies on the server host. If the model omits those keys, the runtime copies the parent run’s **`workspace_root_hint`** (from the chat edge root above) into sub-run context as **`cwd`** so nested `delegate` rounds still resolve hooks.

Cloud chat uses the same **plan subtask** rule as the CLI: put **`plan_subtask_id`** (non-empty string) or **`is_plan_subtask`: true** in the top-level request `context` map to select `when: task_completed` hooks instead of `when: stop`.

For **`POST /chat/stream`**, you may send the same keys as **top-level JSON fields** next to `message` (they are merged into `context` server-side). [`ChatStreamRequest`](../../rust/crates/astra-thin-client/src/protocol.rs) includes optional `plan_subtask_id` and `is_plan_subtask`.

The **astra CLI** embeds **`plan_subtask_id`** and **`is_plan_subtask`** on each **`POST /chat/turn`** body when executing a durable plan subtask so proxies and future server paths can read them without extra client code.

### Plan subtasks

`when: stop` hooks are **not** applied on plan subtasks (avoid duplicating global plan-level verification). Use **`when: task_completed`** for per-subtask checks. Auto-detect `verify-changes` still appends when the subtask message is classified as mutating and `auto_detect` is true.

## Code

- Loader / merge: `rust/crates/runtime/src/turn/stop_hooks_yaml.rs` — `detect_turn_hook_sets`, `project_root_for_stop_hooks` (edge chat), `project_root_from_delegation_context` (delegation sub-runs)
- CLI: `rust/crates/astra-cli/src/cli/chat_stream/sse_loop/mod.rs` calls `astra_runtime::turn::stop_hooks_yaml::detect_turn_hook_sets` with process cwd
- Cloud chat: `rust/crates/runtime/src/server/run_lifecycle.rs` — `build_initial_state` resolves root from `EdgeContext.edge_profile` (`git_root` then `cwd`) and respects `plan_subtask_id` / `is_plan_subtask` in request `context`
- Runtime injection: `rust/crates/runtime/src/turn/agentic_loop_host.rs` (completion + post-delegation), `rust/crates/runtime/src/turn/stop_hooks.rs` — `build_stop_hook_prompt`, `build_teammate_idle_hook_prompt`

## Comparison (Claude Code)

CC supports configurable **stop / task-completed / teammate-idle** style hooks. astra matches all three: **stop** and auto-detect on normal turns, **task_completed** on plan subtasks, **teammate_idle** after delegate rounds.
