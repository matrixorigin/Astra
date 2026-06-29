# Plan Mode

Plan mode is a temporary capability restriction that forces the agent to
research and produce a written plan before touching anything. It is the
guardrail in front of complex, multi-file, or production-affecting work
where letting the model dive in costs more in re-do than in up-front
alignment.

It is **not** a separate chat mode, a separate workflow, or a wrapper
product. It is a permission state with three coupled effects: the schema
hides mutating tools, a system reminder explains the restriction, and
the runtime gate enforces it as a backstop.

## Operating principles

1. **Schema is truth, prompt is hint, runtime is the backstop.** Tools
   the model cannot see, it does not call. Tools the prompt does not
   describe, it forgets. Tools that pass both, the runtime still
   double-checks.
2. **Entering is the user's act, exiting is the model's act.** Users
   press Shift+Tab or run `/permissions plan` to enter. The model calls
   `exit_plan_mode(plan="…")` to surface its proposed plan for user
   approval.
3. **Never block the user.** No cloud, no token, no network, no DB
   record — plan mode still works. Every offline / sub-agent / detached
   path can enter and exit.

## Deployment shapes

The semantic of "am I in plan mode" depends on where execution happens.
Four shapes coexist, each with a distinct truth source.

| Shape           | Description                                            | Tools execute in            | Plan mode truth source                       |
|-----------------|--------------------------------------------------------|-----------------------------|----------------------------------------------|
| **CLI**         | User runs `astra` in a terminal                        | Local edge process          | `perm_manager.mode() == Plan`                |
| **edge-cloud**  | CLI with chat turns proxied through cloud `astra-server` | Local edge process       | `perm_manager.mode() == Plan` + optional cloud mirror |
| **web-agent**   | Browser / background worker, full server-side          | `ServerToolExecutor`        | DB row `agent_sessions.active_plan_id`       |
| **sub-task**    | `spawn_agent` / delegated agents                       | Inherited from parent       | `InheritedPermissions::mode == Plan`         |

The `astra-runtime/server/plan_handlers.rs` module also drives a
separate product called the **plan executor** (multi-subtask durable
runs). It shares the `plans` table with plan mode but operates on
different phases. Plan mode lives in `phase=planning` and `phase=refining`;
the executor lives in `phase=executing`. They never share lifecycle, only
storage.

## User journeys

### A. Shift+Tab to enter (CLI / edge-cloud, primary path)

The most common entry. Zero network calls, no cloud row created.

```
User in TUI → presses Shift+Tab
  ↓
Status line switches to Plan
  ↓
Next turn starts → schema phase prunes mutating tools (read-only +
exit_plan_mode are kept; bash/write/edit/git_commit are dropped)
  ↓
Volatile lane gets [mode=plan] read-only investigation. Author the
plan, then call exit_plan_mode(plan="<markdown>") for user approval.
  ↓
User states the goal → model investigates with read-only tools →
model calls exit_plan_mode(plan="…")
  ↓
TUI opens the plan-review overlay:
  ● Approve & start in auto
  ○ Approve & start in edit
  ○ Approve & start in default
  ○ Keep planning — let me give feedback
  ↓
User picks auto → perm_manager flips to Auto on the next turn boundary →
volatile lane gets [mode=auto] User approved the plan; you are now
executing in auto mode. → model proceeds with the full toolset.
```

### B. `/permissions plan` to enter

Equivalent to Shift+Tab. Provided as a scriptable / discoverable entry.

### C. `enter_plan_mode(goal="…")` tool to enter (cloud workflow)

The model itself decides "this needs a plan first" and calls the tool.

```
Model → enter_plan_mode(goal="Ship auth")
  ↓
ToolExecutor picks the path:
  - Cloud token + active session present → POST /plans (creates
    phase=planning row) and stages Plan in the pending slot
  - Otherwise → no network, just stages Plan in the pending slot
  ↓
Next turn boundary → host reads the pending slot → perm_manager → Plan
  ↓
Continues like journey A.
```

### D. Web-agent

```
Browser submits a goal → server-side agent loop runs
  ↓
Model calls enter_plan_mode(goal="…") → server creates the plans row
and sets agent_sessions.active_plan_id
  ↓
ServerToolExecutor::plan_mode_authoring_active() returns true on
every subsequent tool call → mutating tools are denied
  ↓
Model calls exit_plan_mode(plan="…", approved=true|false)
  - approved=true  → clears active_plan_id, writes unlock
  - approved=false → row stays in planning, awaits another revision
```

Web-agent has no overlay. The model must pass `approved` explicitly.
This is intentional: there is no human at the prompt to make a choice
asynchronously.

## Permission modes

Six permission modes exist underneath. The TUI cycles through five of
them; `Deny` is reserved for headless / CI use and never appears in the
Shift+Tab cycle to prevent accidental entry.

| Permission mode (enum) | TUI label    | Cycle position | Behaviour                                          |
|------------------------|--------------|----------------|----------------------------------------------------|
| `Prompt`               | **Default**  | 1              | Each write/execute tool prompts the user           |
| `AcceptEdits`          | **Edit**     | 2              | Workspace edits auto-approved; shell still prompts |
| `Plan`                 | **Plan**     | 3              | Read-only schema + exit_plan_mode                  |
| `Auto`                 | **Auto**     | 4              | Normal tool risk auto-approved; hard prompts may remain |
| `Bypass`               | **Bypass**   | 5              | Approval prompts skipped; hard denies still apply  |
| `Deny`                 | (hidden)     | —              | All tools denied (CI / harness only)               |

`Shift+Tab` cycles `Default → Edit → Plan → Auto → Bypass → Default`.

## Invariants

These are the contracts pinned by tests under `astra-cli` and
`astra-turn-core`. Violating one is a bug.

### I1. Shift+Tab path makes zero cloud calls

Entering plan mode through Shift+Tab, `/permissions plan`, or an offline
`enter_plan_mode` invocation must never trigger an HTTP request or
create a cloud `plans` row.

> Test: `enter_plan_mode_falls_back_to_local_when_cloud_unavailable`

### I2. Cloud and local paths both stage the same permission change

Whichever path `enter_plan_mode` takes, it stages
`PermissionMode::Plan` in `pending_permission_mode_change` so the host
applies it on the next turn boundary. A cloud row alone is not enough —
without the staged switch the schema would not change for the next turn.

> Test: `enter_plan_mode_stages_perm_mode_change_to_plan_even_on_cloud_path`

### I3. `exit_plan_mode` has dual paths and both work end-to-end

- Cloud path: a `phase=planning` row exists → `POST /plans/{id}/exit-plan-mode`,
  then overlay, then perm_manager pivot.
- Local path: no such row (Shift+Tab entry, offline, network failure) →
  overlay only, then perm_manager pivot.

Either path approves cleanly and stages the next mode.

> Tests: `exit_plan_mode_shift_tab_path_works_without_cloud_plan_record`,
> `exit_plan_mode_local_path_makes_zero_cloud_calls`,
> `exit_plan_mode_overlay_approve_auto_records_pending_mode_change`,
> `exit_plan_mode_overlay_keep_planning_leaves_plan_open`,
> `exit_plan_mode_without_overlay_or_approved_returns_actionable_error`,
> `exit_plan_mode_accepts_plan_alias_and_explicit_approved_skips_overlay`,
> `enter_plan_mode_then_exit_full_cycle_offline`

### I4. The server-side write guard is independent of CLI state

`ServerToolExecutor::plan_mode_authoring_active()` consults only the
DB (`agent_sessions.active_plan_id` plus `plans.phase`). It never reads
the CLI's `perm_manager`, `cloud_plan_mirror`, or any client-side
field. Web-agent behaviour cannot depend on what some other client
machine is doing.

### I5. Sub-tasks inherit plan mode through `InheritedPermissions`

`spawn_agent` propagates plan mode to a child agent via
`InheritedPermissions::mode == Plan`, never through any process-global
state.

### I6. Inside the CLI process, `perm_manager.mode()` is the only truth

Any "am I in plan mode" check in CLI code must go through
`state.plan_mode_active()`, which derives from
`perm_manager.mode() == Plan`.

`state.cloud_plan_mirror.is_some()` is **not** equivalent. The mirror is
populated only on the cloud entry path; Shift+Tab leaves it `None` while
plan mode is fully active.

> Test: `plan_mode_active_tracks_perm_manager_only`

### I7. TUI surfaces derive from `perm_manager`

Status line, footer, follow-up suggestion gate, nudge suppression — all
of them call `state.plan_mode_active()`. None reads
`state.cloud_plan_mirror.is_some()`.

### I8. Mid-turn Shift+Tab does not interrupt the current turn

Pressing Shift+Tab during a turn is allowed. The in-flight LLM request
completes; in-flight tool calls finish. The next turn re-reads
`perm_manager.mode()` and assembles a fresh schema.

### I9. Plan text travels via tool arguments, not state

The model passes the plan as `args.plan` of `exit_plan_mode`. The
overlay shows it. On approve, it is pushed to the volatile lane for the
next turn. It is never written to a separate state field, never
persisted client-side, and never reconstructed from history.

## Tool contracts

### `enter_plan_mode`

```
Args:    { "goal": string (required) }
Result:  Human-readable confirmation including the goal
Effects:
  - Always: pending_permission_mode_change = Some(Plan)
  - Cloud path only: creates a plans row in phase=planning
```

### `exit_plan_mode`

```
Args:
  {
    "plan": string,                // Markdown plan body
    "plan_md" | "plan_markdown":   // Aliases, accepted for compat
    "approved": bool               // Optional: skip the overlay
  }

Effects by case:

  approved=true:
    - Cloud path: POST /plans/{id}/exit-plan-mode { approved: true, plan_md }
    - Local path: overlay only
    - Both: stages the user's chosen mode in pending_permission_mode_change

  approved=false:
    - Cloud path: POST /plans/{id}/exit-plan-mode { approved: false, plan_md }
    - Local path: result message only
    - Neither stages a permission change

  approved missing + overlay sink available:
    Opens the plan-review overlay (4-way radio).

  approved missing + no overlay sink (headless):
    Returns an actionable error asking the caller to retry with
    explicit approved=true|false.
```

### Permission engine exemption

`PLAN_MODE_REQUIRED_TOOLS = ["enter_plan_mode", "exit_plan_mode"]`.

These two tools always pass the plan-mode gate at runtime, otherwise the
model would have no way to leave plan mode. The same constant drives
`tool_schema_prune::plan_mode_restrictions`, so the schema and the
runtime agree about which tools survive plan mode.

## Turn-boundary contract

Every turn starts at `cli_loop_host::execute_turn`:

1. **Apply pending permission mode**: read
   `pending_permission_mode_change`, call `perm_manager.set_mode(...)`,
   refresh the TUI footer/status line.
2. **Install the overlay channel**: hand the current turn's
   `ask_user_request_tx` to the ToolExecutor so tools can open the
   overlay.
3. **Inject the plan-mode marker**: if `state.plan_mode_active()`, push
   a `PlanModeMarker` onto the volatile lane. The kind is a singleton —
   re-pushing on subsequent turns coalesces, never stacks.

Every turn ends at `on_turn_completed`:

4. **Uninstall the overlay channel** so a stale sender never leaks into
   background sub-runs that share the same `Arc<ToolExecutor>`.

Mid-turn writes to `pending_permission_mode_change` are deliberate: they
take effect on the next boundary, never mid-flight. This keeps in-flight
tool calls coherent (I8).

## Known boundaries and gaps

### Stable behaviours

- Calling `exit_plan_mode` outside plan mode is not silently dropped: the
  overlay still opens (when a sink is present), surfacing the
  out-of-context invocation to the user.
- Producing markdown without calling `exit_plan_mode` is **not** an
  implicit approval. The user has to pick another exit path (Shift+Tab,
  `/permissions <mode>`).
- Cancelling the overlay (Esc / closed channel) is equivalent to "Keep
  planning": no approval, no mode switch.

### Open work

- **Stop-hook nudge for missed `exit_plan_mode`**: when the model
  produces a plan-shaped response in plan mode but does not call the
  tool, inject a corrective nudge on the next turn.
- **CLI / Web entry unification — assessed, deferred:** the two
  surfaces use distinct truth sources by design (I4 / I6). Forcing
  unification today would require all four of: (a) every Shift+Tab
  also creating a `plans` row in the cloud (breaks I1 — zero cloud
  calls); (b) every cloud row materializing a CLI `perm_manager`
  pivot (breaks I4 — server independence); (c) a sync protocol that
  resolves "user pressed Shift+Tab in CLI while a web session
  approved a plan in flight" (no obvious right answer); (d) a
  cross-surface session ID that survives client handoff. None of
  those carry weight today: web-agent runs and CLI runs do not share
  sessions in production, only in conceptual "what if" scenarios.
  The current decoupling is the correct stance until a concrete
  cross-surface flow demands it. The eventual contract — if and
  when it is needed — should be: when a session opens both surfaces
  concurrently, treat the cloud row as authoritative and have the
  CLI follow it on poll, never the reverse. CLI-only Shift+Tab
  remains zero-network (Invariant I1) for the no-cloud and offline
  paths.

## Test coverage map

| Invariant | Test                                                                       | File                                                                  |
|-----------|----------------------------------------------------------------------------|-----------------------------------------------------------------------|
| I1        | `enter_plan_mode_falls_back_to_local_when_cloud_unavailable`               | `astra-cli/src/edge_tools/tests/executor_core_tests.rs`               |
| I1        | `exit_plan_mode_local_path_makes_zero_cloud_calls`                         | same                                                                  |
| I2        | `enter_plan_mode_stages_perm_mode_change_to_plan_even_on_cloud_path`       | same                                                                  |
| I3        | `exit_plan_mode_shift_tab_path_works_without_cloud_plan_record`            | same                                                                  |
| I3        | `exit_plan_mode_overlay_approve_auto_records_pending_mode_change`          | same                                                                  |
| I3        | `exit_plan_mode_overlay_keep_planning_leaves_plan_open`                    | same                                                                  |
| I3        | `exit_plan_mode_without_overlay_or_approved_returns_actionable_error`      | same                                                                  |
| I3        | `exit_plan_mode_accepts_plan_alias_and_explicit_approved_skips_overlay`    | same                                                                  |
| I3 cycle  | `enter_plan_mode_then_exit_full_cycle_offline`                             | same                                                                  |
| I4        | `plan_mode_authoring_active_*`                                             | server-side `server_tool_executor` tests                              |
| I6 + I7   | `plan_mode_active_tracks_perm_manager_only`                                | `astra-cli/src/cli/session_state.rs`                                  |
| Engine    | `plan_mode_allows_plan_control_tools_so_model_can_exit`                    | `astra-turn-core/src/permission_engine.rs`                            |
| Schema    | `plan_mode_keeps_read_only_and_required_tools`                             | `astra-turn-core/src/tool_schema_prune.rs`                            |

## Implementation map

| File                                                                            | Responsibility                                                                                  |
|---------------------------------------------------------------------------------|-------------------------------------------------------------------------------------------------|
| `astra-cli/src/cli/session_state.rs`                                            | `cloud_plan_mirror` field, `plan_mode_active()` helper, I7 test                                 |
| `astra-cli/src/edge_tools.rs`                                                   | `enter_plan_mode_remote` / `exit_plan_mode_remote` dual paths, `pending_permission_mode_change` |
| `astra-cli/src/cli/chat_stream/sse_loop/cli_loop_host.rs`                       | Turn-boundary application of pending mode, overlay install, marker push                         |
| `astra-cli/src/cli/chat_stream/sse_loop/agentic_loop_turn.rs`                   | Schema-phase plan-mode pruning                                                                  |
| `astra-turn-core/src/tool_schema_prune.rs`                                      | `PLAN_MODE_REQUIRED_TOOLS`, `plan_mode_restrictions`                                            |
| `astra-turn-core/src/permission_engine.rs`                                      | Plan-mode short-circuit + plan-control exemption                                                |
| `astra-runtime/src/turn/agentic_loop_host.rs`                                   | `VolatileKind::PlanModeMarker` singleton                                                        |
| `astra-runtime/src/server/server_tool_executor.rs`                              | Server-side write guard, independent from CLI state                                             |
| `astra-runtime/src/server/plan_handlers.rs`                                     | `/plans` HTTP handlers (cloud workflow backend)                                                 |
