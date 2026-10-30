# Current capability status

This reference describes the implementation in **this source revision**. Read
[the workspace version](../../Cargo.toml) in the same checkout. Select a release
tag or exact source SHA to evaluate that version; `main` is a moving target.
Release notes link to the immutable source snapshot of this page. Snapshots
begin with releases whose selected source contains this reference.

Target contracts remain in [the design index](../design/README.md). This page
owns current availability, profiles, limits and evidence; the
[coverage matrix](../testing/coverage-matrix.md) owns the detailed test inventory.
Neither a test's existence nor a green build proves production maturity.

## Reading the matrix

- **Beta**: an implemented, supported path with operational requirements and
  evolving contracts. No row claims a production stability certification.
- **Partial**: the named subset works; the limits column identifies the missing
  part of the broader capability.
- **Experimental**: explicitly selected evaluation or policy behavior with no
  general benefit or reliability guarantee.
- **Design-only**: a target contract without an available execution path.
- **Known-broken**: a demonstrated defect, linked to its tracking issue.

Profiles identify supported surfaces: **CLI**, **Server**, **Web**,
**User Runner**, and **Offline**. CLI/Web agent execution requires the Server;
local MCP configuration and offline journal analysis are exceptions. A User
Runner is explicitly selected and bound; Server-only mode has no ambient access
to user files, shell, Git, credentials or private networks. Dependencies and
policy still apply when a capability is enabled by default.

Verification names a lane and its source, not an execution result. **Not run**
means this reference makes no claim that the live infrastructure, installed
platforms or provider journey was executed while preparing it. Use the release's
CI artifacts and environment-specific qualification for run evidence.

All rows have **0.x** compatibility: follow the exact source/release reference
and its release notes. Introduction versions and cross-version stability are
not inferred from current code. Existing authentication and consumer contracts
still apply; this matrix does not introduce a compatibility mode.

## Capabilities

| Capability | Status | Profiles | Default / flags | Dependencies | Implementation | Verification lane | Run evidence | Limits / tracking | Compatibility |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Agent chat | Beta | CLI, Server, Web | Enabled after authentication and model configuration | MatrixOne, admitted model; Web build for Web | [Shared loop](../../crates/runtime/src/turn/agentic_loop/mod.rs) | [Offline HTTP agent journey](../../crates/runtime/tests/web_agent_e2e.rs) | Not run | Provider availability, quotas and selected tool bindings constrain execution | 0.x |
| ChatGPT subscription inference | Experimental | CLI, Server, Web | Disabled; `ASTRA_CHATGPT_SUBSCRIPTION_ENABLED=1` and stable host ID | MatrixOne, token encryption key, explicit OpenAI sign-in/plan grant | [Connection owner](../../crates/services/src/chatgpt.rs), [Responses transport](../../crates/runtime/src/turn/llm/responses.rs) | [OAuth/storage journey](../../crates/services/tests/chatgpt_connections_db_it.rs), [runtime checkpoint journey](../../crates/runtime/src/turn/llm/responses/tests.rs) | Local fake-provider/database coverage; live OpenAI journey not run | CLI linking only; same-host personal self-hosting, text/function tools; [deployment limits](configuration.md#chatgpt-subscription-connections), [#1158](https://github.com/matrixorigin/Astra/issues/1158) | Catalog v2 retained; upgrade all replicas before enabling |
| Durable Work | Partial | CLI, Server, Web | Work APIs available; background execution requires configured workers | MatrixOne, model and execution owner | [Run engine](../../crates/runtime/src/server/run/engine.rs) | [Live DB task/run journeys](../../crates/runtime/tests/system_matrix_http_e2e/journey_tasks_runs.rs) | Not run | Durable control and recovery paths exist; transport-loss incident causality remains open in [#987](https://github.com/matrixorigin/Astra/issues/987) | 0.x |
| Checkpoints and recovery | Partial | CLI, Server, Web | Persistence enabled; recovery subject to lease and owner authority | MatrixOne; local journal for selected CLI state | [Restore owner](../../crates/services/src/session_restore.rs) | [Offline restore tests](../../crates/services/src/session_restore.rs), [live persistence journey](../../crates/runtime/tests/system_matrix_http_e2e/journey_stream_persistence.rs) | Not run | Server resume requires canonical Context; cloud checkpoint-only fallback is retired. CLI local recovery remains explicit; external side effects are not restored; investigate lease incident [#906](https://github.com/matrixorigin/Astra/issues/906) separately | 0.x |
| Context and cache | Beta | CLI, Server, Web | Budgeted assembly and configured compaction enabled; cache protocol depends on Offering | Model context budget and provider cache support | [Wire assembly](../../crates/runtime/src/turn/wire_assembly.rs) | [Mechanical pipeline](../../crates/runtime/tests/compaction_pipeline.rs), [cache reporting](../../crates/astra-cli/src/cli/turn/turn_reporting.rs) | Not run | Token savings are estimates; cache rate requires complete primary input usage, excludes output/auxiliary calls, and is not a latency guarantee | 0.x |
| Model access | Beta | CLI, Server, Web | Explicit configured Offering required | Provider credentials, model catalog and policy | [Execution admission](../../crates/runtime/src/server/model_execution_admission.rs) | [Offline agent journey](../../crates/runtime/tests/web_agent_e2e.rs) | Not run | Configured inference is available; durable Runner-owned inference is separate work ([#702](https://github.com/matrixorigin/Astra/issues/702)) | 0.x |
| Routing | Experimental | CLI, Server, Web | Adaptive routing is opt-in | Qualified models, routing policy and evaluation data | [Routing owner](../../crates/runtime/src/server/server_loop_host/model_routing.rs) | [Routing tests](../../crates/runtime/src/server/server_loop_host/model_routing/tests.rs) | Not run | Quality, cost and latency improvements require paired evidence ([#764](https://github.com/matrixorigin/Astra/issues/764)) | 0.x |
| Judgment advice | Experimental | CLI, Server, Web | Optional separately configured judgment model | Admitted ordinary LLM or Jev; caller-specific policy | [Typed judgment contract](../../crates/astra-turn-types/src/judgment.rs) | [Reproducible memory evaluation](../../expriment/jev-memory/SKILL.md) | Not run | Bounded typed advice does not grant execution authority; no whole-agent benefit claim ([#862](https://github.com/matrixorigin/Astra/issues/862)) | 0.x |
| Skills and hooks | Partial | CLI, Server, User Runner | Skills discovered from configured sources; hooks require explicit configuration | Selected skill provider and permitted executor | [Skill owners](../../crates/astra-skills/src/lib.rs), [hooks](../../crates/astra-skills/src/hooks.rs) | [Hook unit tests](../../crates/astra-skills/src/hooks.rs) | Not run | Parser/diagnostic behavior exists; hook enforcement and transformation roadmap remains [#991](https://github.com/matrixorigin/Astra/issues/991) | 0.x |
| Provider admission | Beta | CLI, Server, Web, User Runner | Enforced for admitted tool execution | Authenticated owner, binding, capability and policy | [Tool admission](../../crates/runtime/src/server/tool_admission.rs) | [Offline agent/provider journey](../../crates/runtime/tests/web_agent_e2e.rs) | Not run | Discovery/schema visibility alone grants no execution authority; offline or missing binding blocks that capability | 0.x |
| User Runner execution | Partial | CLI, Server, User Runner | Explicit local CLI or Edge binding; absent in Server-only mode | Runner process, authorized workspace, local toolchain/network | [Execution boundary](../design/edge-runtime-tool-boundary.md), [CLI executor](../../crates/astra-cli/src/edge_tools.rs) | [Shell boundary integration](../../crates/astra-cli/tests/shell_execution_boundaries.rs) | Not run | Platform isolation and installed-tool qualification vary; selecting a Runner does not imply the pending durable inference contract in [#702](https://github.com/matrixorigin/Astra/issues/702) | 0.x |
| MCP configuration | Beta | CLI, Offline | Local `astra mcp` commands available; project scope by default | Selected local filesystem; installed CLI | [Configuration owner](../../crates/astra-cli/src/cli/mcp_config.rs), [loader](../../crates/astra-cli/src/manifest_loader.rs) | [CLI configuration tests](../../crates/astra-cli/src/cli/mcp_config.rs) | Not run | [Canonical setup reference](cli-commands.md#mcp-configuration): cooperating writers serialize per destination; configuration is not installation or connectivity; changes apply at next chat startup; `add-json` accepts one entry and does not fully validate server semantics | 0.x |
| MCP tool execution | Beta | CLI, Server, User Runner | Explicitly configured and bound server only | MCP server, transport credentials and policy | [MCP client](../../crates/astra-mcp/src/lib.rs), [Server MCP](../../crates/runtime/src/server/runtime_mcp.rs) | [MCP transport tests](../../crates/astra-mcp/src/connection.rs) | Not run | `test`/`ping` launch configured processes or contact networks; scope-specific checks do not prove the effective winner; cross-repository cancellation settlement remains [#1016](https://github.com/matrixorigin/Astra/issues/1016) | 0.x |
| Permissions and sandbox | Partial | CLI, Server, User Runner | Admission/permission rules enforced; isolation selected by deployment and platform | Host sandbox support, selected executor and configured policy | [Sandbox owner](../../crates/astra-sandbox/src/lib.rs) | [Shell boundary integration](../../crates/astra-cli/tests/shell_execution_boundaries.rs) | Not run | Metadata-only Sandbox CRUD is retired. Approval is not OS isolation; no universal containment guarantee across hosts or remote MCP servers | 0.x |
| Replay and versioning | Partial | CLI, Server, Offline | Recorded journal/API facts available; optional captures must be enabled | Canonical journal or MatrixOne, required retained artifacts | [Journal owner](../../crates/services/src/session_journal.rs) | [Public CLI journal integration](../../crates/astra-cli/tests/cli_journal.rs) | Not run | Event replay and context dumps do not reproduce arbitrary external effects or the exact provider request unless that evidence was captured | 0.x |
| Observation and audit | Partial | CLI, Server, Web | Structured facts enabled; raw prompt/debug capture opt-in | Retained execution facts, configured persistence | [Explain](../../crates/astra-cli/src/tui/history_cell/explain_analyze.rs), [observability](../../crates/runtime/src/observability/hub.rs) | [Explain rendering tests](../../crates/astra-cli/src/tui/history_cell/explain_analyze.rs), [context dump tests](../../crates/astra-cli/src/cli/context_dump.rs) | Not run | Separate skill-selection SQL audit is retired; tool traces do not prove selection judgments. Unavailable evidence is not zero work; unwired compression/explanation projections are retired; introspection/reflection cannot reconstruct unrecorded facts | 0.x |
| Memory | Beta | CLI, Server, Web | Enabled when configured; retrieval has caller-specific fallbacks | Memoria and configured embedding/retrieval services | [Memory selection](../../crates/runtime/src/memory_hooks/relevance.rs) | [Memory pilot](../guides/memory-judgment-pilot.md), [live full journey](../../crates/runtime/tests/system_matrix_http_e2e/journey_full.rs) | Not run | A passing offline selection test does not qualify a live Memoria installation or prove relevance for arbitrary tasks | 0.x |
| Evaluation and learning | Experimental | Server, Offline | Explicit evaluation/tuning workflows; no unconditional automatic promotion | Evidence datasets, configured storage/model and evaluation policy | [Evaluation contract](../design/evaluation-and-learning.md), [evaluation sources](../../expriment/jev-memory/SKILL.md) | [Offline routing workflow](../guides/model-router-offline.md) | Not run | Existing evaluations do not establish the pending evidence-backed comparison/promotion platform ([#842](https://github.com/matrixorigin/Astra/pull/842)) or all strategy benefits ([#862](https://github.com/matrixorigin/Astra/issues/862)) | 0.x |
| MOI device sign-in | Experimental | CLI | Explicit `astra login --device-code`; UC deployment must enable device authorization | UC/Keycloak device bridge, trusted MOI and Astra endpoints | [Native UC login](../../crates/astra-cli/src/cli/auth_flow/uc/device_code.rs) | [Polling and endpoint contract tests](../../crates/astra-cli/src/cli/auth_flow/uc/device_code/tests.rs) | Not run | Headless entrypoint; deployment and complete product bootstrap qualification required; no automatic fallback or longer session lifetime ([design](../guides/moi-native-login.md#sign-in-from-a-headless-terminal)) | 0.x |

## Keeping claims accurate

A public capability or profile change must update the affected row or explain
why the existing assessment still applies in the PR's capability assessment.
Every PR must start with the raw metadata line
`Capability assessment: <disposition>: <reason>`, choosing exactly `Updated`,
`Unchanged` or `N/A` and replacing the reason with an explanation. Start at
character zero, with the literal spaces shown, and keep the line within 1,024
Unicode characters (excluding its LF or CRLF terminator). Leading blank lines,
indentation and BOM are rejected. A reason cannot be empty or a placeholder.
Only this first line is authoritative; later labels and Markdown content are
ignored. Markdown escapes, code and HTML-like text are raw data, including in
the reason; the check does not promise rendered visibility.

The existing required `Check PR title` job validates this field on PR creation,
body edits and source updates without checking out code. Dependency and release
PRs follow the same rule; maintainers complete the line when automation does not
supply it. `Updated` identifies affected capabilities, status/profiles and
reference changes; `Unchanged` explains why the existing assessment remains
accurate. A justified `N/A` is appropriate only when there is no capability
impact. Review owns the accuracy and completeness of this explanation.
Record the implementation and meaningful verification lane; publish run results
only with their revision, environment and artifact. Keep secrets and private
endpoints out of evidence links.

The existing repository CI validates the core-row inventory, field shape,
status/profile vocabulary and local links. Review owns semantic completeness:
CI does not infer product maturity or scan arbitrary prose/routes for new claims.
The matrix is documentation, not a runtime catalog or a source of admission
permissions. See [release snapshot ownership](../guides/releasing.md).
