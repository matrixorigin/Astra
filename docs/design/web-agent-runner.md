# Web-Agent Runner

> Status: Proposed design contract.
> Scope: Self-hosted execution for Web agent sessions, enabling cloud-only state with user-hosted runtime environments.
> Audience: Product, runtime, edge, security, and observability maintainers.

This document defines the product and runtime design for **Web-Agent Runner**: a self-hosted execution surface that allows Web agent sessions to operate on user-owned codebases, private infrastructure, and specialized hardware while maintaining cloud-only session state.

## Intent

Web agent sessions today run entirely in cloud sandboxes. This works for public repositories and generic development tasks, but fails for:

- Private codebases behind firewalls
- Internal APIs and databases
- Specialized hardware (GPU clusters, FPGAs, embedded devices)
- Compliance requirements that forbid code leaving the network

The solution is not to abandon the Web agent model, but to decouple **state** from **execution**:

- **State is cloud-only**: session history, traces, plans, memory, and artifacts live in the cloud database.
- **Execution is user-hosted**: a self-hosted runner (the `astra-edge` binary) connects to the cloud and executes tools locally.

This gives users the best of both worlds: Web agent convenience (multi-device access, persistent state, collaborative features) with local execution power (filesystem access, private network, custom tooling).

### Two Execution Paradigms

Astra supports two complementary execution paradigms for Web agent sessions:

| Paradigm             | Mechanism                                                                       | Use Case                                                                                       |
| -------------------- | ------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------- |
| **Runner Pattern**   | Self-hosted runtime executes shell commands, file operations, and git workflows | Heavy operations: code compilation, testing, deployment, database migrations                   |
| **MCP Tool Pattern** | Remote MCP tools expose lightweight operations via API                          | Light operations: database queries, API calls, monitoring alerts, external service integration |

Both can be composed in a single session turn: the agent might use MCP tools to query a production database, then dispatch a runner task to run tests against the results.

## Goals

- **Runner registration**: users can register self-hosted runners with labels (dev, gpu, staging) and capabilities (shell, docker, k8s)
- **Persistent connection**: runners maintain a WebSocket long connection to the cloud with automatic reconnection
- **Task dispatch**: the cloud routes execution tasks to the best available runner based on labels, capabilities, and load
- **Execution streaming**: stdout, file diffs, and structured tool results stream back to the cloud in real-time
- **Multi-device resume**: any device can open the same session and see consistent execution state
- **Security**: runners execute in sandboxed environments with credential isolation and audit logging
- **Multi-runner support**: users can register multiple runners per workspace (dev, staging, production)
- **MCP integration**: lightweight operations use MCP tools without requiring a runner
- **Audit trail**: all executed commands and tool calls are recorded in `agent_events` for debugging and compliance

## Non-Goals

- **Not a CI/CD system**: runners execute agent tasks, not pipeline DAGs. No workflow orchestration beyond single-session scope.
- **Not a container orchestrator**: no Kubernetes scheduling, pod management, or cluster coordination.
- **No arbitrary user code in v1**: runner configuration is declarative (labels, capabilities, sandbox policy). No custom scripts or plugins.
- **No peer-to-peer runner communication**: all coordination flows through the cloud.
- **Not replacing CLI mode**: local CLI execution remains a first-class experience for single-user, single-terminal workflows.

## Competitive Landscape: OpenAI Codex (May 2026)

In May 2026, OpenAI launched [Work with Codex from anywhere](https://openai.com/index/work-with-codex-from-anywhere/), bringing Codex to the ChatGPT mobile app (iOS/Android). This is the closest existing product to Astra's Web Agent + Runner vision and validates the architecture from multiple angles.

### Codex Architecture

```
iPhone (ChatGPT App) ──→ OpenAI Cloud (Codex Agent) ──→ Mac (Codex Desktop)
                                │
                          Secure Relay
                          (no public IP needed)
```

- **Mac runs Codex Desktop** as the execution environment (analogous to Astra runner).
- **Secure relay** connects Mac to OpenAI Cloud without exposing the machine to the public internet.
- **ChatGPT mobile app** connects to the cloud and receives real-time state sync.
- **State is cloud-native**: threads, approvals, plugins, project context live in the cloud, synced to all connected devices.

### What Codex Mobile Can Do

| Capability                               | Codex                     | Astra (planned)         |
| ---------------------------------------- | ------------------------- | ----------------------- |
| Approve/reject agent commands            | ✅                        | ✅                      |
| View diffs, screenshots, terminal output | ✅                        | ✅                      |
| Start new threads, switch projects       | ✅                        | ✅                      |
| Switch AI models                         | ✅                        | ✅                      |
| Multi-device simultaneous connection     | ✅                        | ✅                      |
| Edit code on mobile                      | ❌ (intentional)          | ❌ (same design choice) |
| Local file operations                    | ❌ (files stay on runner) | ❌ (same design choice) |
| Offline use                              | ❌                        | ❌                      |

### Astra's Differentiation Opportunities

| Dimension                 | Codex                                        | Astra                                                                                                          |
| ------------------------- | -------------------------------------------- | -------------------------------------------------------------------------------------------------------------- |
| **Runner portability**    | Mac desktop app only (Windows "coming soon") | Any Linux/macOS via Docker, VM, or bare metal                                                                  |
| **Multi-runner routing**  | One Mac = one execution target               | Multiple runners with labels (dev, gpu, ci), dynamic dispatch                                                  |
| **Workspace persistence** | Task-level sandbox, ephemeral                | **Persistent workspace** with cross-session state accumulation (cached deps, build artifacts, project context) |
| **Session fork & merge**  | Not available                                | First-class fork/merge for parallel exploration                                                                |
| **Collaboration**         | Single-user                                  | Multi-user session sharing, async handoff                                                                      |
| **MCP integration**       | Not available                                | MCP tools as lightweight alternative to full runner                                                            |
| **Pricing model**         | Free tier (OpenAI Cloud)                     | Self-hosted runners reduce cloud compute cost                                                                  |
| **Enterprise**            | HIPAA (Enterprise only), SSH, access tokens  | Self-hosted runners keep code on-prem by default                                                               |

### Key Takeaways for Astra

1. **Market validation**: Codex mobile proves "phone for decisions, computer for execution" is a real product, not a speculative idea. The 20k-star open-source project [happy](https://github.com/slopus/happy) (pre-dating official mobile support) confirms strong latent demand.

2. **Astra's structural advantage**: Codex ties execution to a specific Mac running Codex Desktop. Astra's runner abstraction (Docker/VM) is inherently more portable and multi-tenant. This is the wedge — Codex can't easily do "my GPU server and my CI box and my laptop, all from the same Web UI."

3. **The gap to close**: Codex's secure relay and pairing UX (scan QR → authorize → done) is polished. Astra's runner registration UX must match or exceed this simplicity.

4. **Don't compete on mobile editing**: Both Codex and Astra agree — mobile is for decision-making, not code authoring. Invest in diff viewing, approval flows, notifications, and context switching.

5. **Workspace persistence is the killer differentiator**: Codex sandboxes are ephemeral. Every session starts fresh. Astra's persistent workspace with accumulated state (dependencies, build cache, project knowledge) creates switching cost and compounding value over time.

## Execution Paradigms

### Runner Pattern

A runner is a self-hosted instance of the `astra-edge` binary that connects to the Astra cloud and executes tools on behalf of Web agent sessions.

**User mental model**: GitHub Actions runner. The user registers a runner, assigns it labels, and the system dispatches tasks to it automatically.

**When to use a runner**:

- The task requires filesystem access (code compilation, file editing, git operations)
- The task requires private network access (internal APIs, databases, services)
- The task requires specialized hardware (GPU, TPU, FPGA)
- The task produces large artifacts (binaries, logs, datasets)

**Runner capabilities**:

- Shell command execution (bash, zsh, PowerShell)
- File read/write with path policy enforcement
- Git operations (clone, commit, push, branch management)
- Docker container management (build, run, stop)
- Process management (spawn, monitor, kill)

### MCP Tool Pattern

MCP (Model Context Protocol) tools are remote operations exposed via the MCP server. The agent calls these tools directly without requiring a runner.

**When to use MCP tools**:

- The operation is lightweight (database query, API call, webhook trigger)
- The operation does not require filesystem access
- The operation is idempotent and stateless
- The operation targets an external service (Slack, Jira, monitoring system)

**MCP tool examples**:

- `query_database(sql)` — run a read-only SQL query
- `send_slack_message(channel, text)` — post a message to Slack
- `get_jira_issue(issue_id)` — fetch Jira issue details
- `create_github_issue(title, body)` — open a GitHub issue

### Composition

A single session turn can combine both paradigms:

```text
User: "Check if the production database has any orphaned records, and if so, run the cleanup script."

Agent turn:
  1. MCP tool: query_database("SELECT COUNT(*) FROM orphaned_records")
     → Result: 42 orphaned records
  2. Runner task: shell("cd /opt/scripts && ./cleanup_orphaned_records.sh --dry-run")
     → Result: "Would delete 42 records"
  3. Agent asks user for approval
  4. Runner task: shell("./cleanup_orphaned_records.sh --execute")
     → Result: "Deleted 42 records"
```

### Decision Matrix

| Criterion                    | Runner          | MCP Tool         |
| ---------------------------- | --------------- | ---------------- |
| Filesystem access            | ✅ Required     | ❌ Not supported |
| Private network              | ✅ Required     | ❌ Not supported |
| Specialized hardware         | ✅ Required     | ❌ Not supported |
| Large artifacts              | ✅ Required     | ❌ Not supported |
| Lightweight operation        | ⚠️ Overkill     | ✅ Preferred     |
| External service integration | ⚠️ Possible     | ✅ Preferred     |
| Idempotent operation         | ⚠️ Not required | ✅ Required      |
| Real-time streaming          | ✅ Required     | ❌ Not supported |

### Architecture

```text
┌─────────────────────────────────────────────────────────┐
│  Web UI / CLI                                           │
│  (browser or terminal)                                  │
└─────────────────────────────────────────────────────────┘
                        │
                        ▼
┌─────────────────────────────────────────────────────────┐
│  Cloud API Server                                       │
│  - Auth, session state, task dispatch                   │
│  - Runner registry (DB: runners table)                  │
│  - Task router (capability match → runner selection)    │
└─────────────────────────────────────────────────────────┘
                        │
            ┌───────────┴───────────┐
            │                       │
            ▼                       ▼
┌──────────────────────┐  ┌──────────────────────┐
│  Self-Hosted Runner  │  │  MCP Server          │
│  (astra-edge binary) │  │  (remote tools)      │
│                      │  │                      │
│  - WebSocket connect │  │  - HTTP/gRPC API     │
│  - Shell execution   │  │  - Stateless ops     │
│  - File operations   │  │  - External services │
│  - Git workflows     │  │                      │
│  - Sandbox isolation │  │                      │
│  - Result streaming  │  │                      │
└──────────────────────┘  └──────────────────────┘
```

## Runner Lifecycle and Dispatch

### Registration

A user registers a runner by running `astra-edge register` on their machine:

```bash
astra-edge register \
  --workspace my-workspace \
  --labels dev,gpu \
  --capabilities shell,docker,git
```

The CLI:

1. Authenticates with the cloud API using the user's token
2. Generates a unique runner ID
3. Registers the runner in the `runners` table with labels and capabilities
4. Establishes a WebSocket connection to the cloud
5. Begins sending heartbeats

### Heartbeat and Health

Runners send heartbeats every 30 seconds. The cloud updates `runners.last_heartbeat` on each heartbeat.

**Runner states**:

- `offline` — no heartbeat in 60 seconds
- `idle` — heartbeat received, no active task
- `busy` — executing a task
- `draining` — finishing current task, not accepting new tasks (graceful shutdown)

The cloud marks a runner as `offline` if no heartbeat is received for 60 seconds. Offline runners are excluded from task dispatch.

### Task Dispatch

When a Web agent session needs to execute a tool, the cloud:

1. Queries `runners` for idle runners matching the required labels and capabilities
2. Selects the runner with the lowest load (fewest active tasks)
3. Sends the task to the runner via WebSocket
4. Updates `runners.status` to `busy`
5. Records the task in `runner_tasks` table

**Runner selection algorithm**:

```text
SELECT * FROM runners
WHERE status = 'idle'
  AND labels @> required_labels
  AND capabilities @> required_capabilities
ORDER BY active_tasks ASC, last_heartbeat DESC
LIMIT 1
```

If no runner matches, the task fails with "No available runner" and the agent informs the user.

### Execution Streaming

The runner streams execution results back to the cloud in real-time:

- **stdout/stderr**: chunked text output (every 100ms or 1KB)
- **File diffs**: structured diff objects (file path, old content, new content)
- **Tool results**: structured JSON (exit code, output, artifacts)

The cloud:

- Stores full output in `runner_tasks.output` for audit
- Summarizes output for the LLM prompt (first 100 lines + last 100 lines)
- Streams output to the Web UI for real-time display

### Resume and Reattachment

If a runner disconnects mid-task:

- The task remains in `runner_tasks` with status `running`
- The runner reconnects and resumes the task (if the process is still alive)
- If the runner cannot resume, the task is marked `failed` and the agent is notified

If a user opens the same session from another device:

- The Web UI queries `agent_events` and `runner_tasks` to reconstruct execution state
- Real-time output streams via WebSocket to all connected devices

## Security and Isolation

### Runner Authentication

- Runners authenticate with a registration token (one-time use, expires in 1 hour)
- After registration, runners use a long-lived WebSocket token (rotated every 24 hours)
- Tokens are scoped to a specific workspace and user

### Execution Sandboxing

Runners execute tools in a sandboxed environment using `astra-sandbox`:

- **Process isolation**: each shell command runs in a separate process group
- **Path policy**: file operations are restricted to the workspace directory (configurable)
- **Network policy**: outbound network access is restricted to allowlisted domains (configurable)
- **Resource limits**: CPU, memory, and disk quotas enforced via cgroups

### Credential Isolation

- Runners never receive cloud database credentials
- Runners never receive LLM API keys
- Runners only receive the minimal credentials needed for their tasks (e.g., git SSH key for the workspace repository)

### Audit Trail

All executed commands and tool calls are recorded in `agent_events`:

```json
{
  "event_type": "runner_tool_call",
  "runner_id": "runner_abc123",
  "task_id": "task_xyz789",
  "tool_name": "shell",
  "tool_args": { "command": "cargo build --release" },
  "exit_code": 0,
  "stdout_summary": "Compiling astra v0.1.0...",
  "duration_ms": 12345,
  "timestamp": "2026-05-31T10:00:00Z"
}
```

## Existing Foundations and Gaps

### What Already Exists

- **astra-edge binary**: WebSocket connection, tool execution, automatic reconnection
- **thin-client protocol**: `EdgeRegisterRequest`, `EdgeToolCallRequest`, `EdgeToolCallResult`
- **sync engine**: push/pull state sync between edge and cloud
- **astra-sandbox**: process isolation, path policy, shell hardening
- **astra-mcp**: MCP server implementation with tool registry
- **agent_events**: structured event logging for audit and debugging
- **session fork/resume**: cloud-only session state with checkpoint/restore

### What's Missing

- **Persistent runner entity**: current `astra-edge` is ephemeral (no `runners` table, no heartbeat, no status tracking)
- **Task router**: current dispatch is static (`edge_executor_id` is specified by the client, not selected by the cloud)
- **Runner management UI**: no Web UI page for registering runners, viewing status, or browsing execution history
- **Multi-terminal sync**: Web UI does not yet stream real-time execution output to multiple connected devices
- **Runner scoping**: no user/team/workspace ownership model for runners

### Estimated Effort

| Component                  | Effort     | Description                                                              |
| -------------------------- | ---------- | ------------------------------------------------------------------------ |
| Runner registry (DB + API) | 3 days     | `runners` table, registration API, heartbeat endpoint, status management |
| Task router                | 2 days     | Runner selection algorithm, task dispatch logic, retry policy            |
| Runner management UI       | 2 days     | Registration flow, status dashboard, execution history                   |
| Multi-terminal sync        | 2 days     | WebSocket channel for real-time output streaming to multiple browsers    |
| **Total**                  | **9 days** |                                                                          |

### Open Product Questions

1. **Runner lifecycle model**: Should runners be always-on (like GitHub Actions self-hosted runners) or on-demand (spawned when a task arrives, terminated when idle)?
2. **Multi-tenant sharing**: Should a team be able to share runners, or is each runner scoped to a single user?
3. **Resource quotas**: How should runner execution be metered and billed (CPU-hours, task count, wall-clock time)?
4. **Auto-provisioning**: Should Astra offer to spawn cloud VMs as runners (like GitHub-hosted runners), or is self-hosted the only option?
5. **Runner capabilities schema**: How should capabilities be defined (free-form strings, typed enums, capability manifests)?

## Runner Registration Protocol

### Overview

Runner registration follows a four-step flow:

1. **Token Generation**: User generates a runner registration token via Web UI or CLI
2. **Runner Deployment**: User runs `astra-edge register` with the token
3. **Cloud Validation**: Cloud verifies token and creates runner record
4. **Runner Activation**: Runner connects via WebSocket and becomes available for task routing

**CLI path** (for developers):

```bash
astra runner token create --name "my-laptop" --labels "dev,linux"
# Output: runner_token=rn_abc123def456
```

Token structure:

- Format: `rn_` prefix + 24 character random string
- Scope: bound to user_id, workspace_id
- Expiry: 24 hours by default, configurable
- Single-use: invalidated after first successful registration

### Step 2: Deploy Runner

**Option A: One-Click Installer (Non-Developer Friendly)**

For users who prefer GUI installation:

1. Web UI → Add Runner → Select platform (macOS / Linux / Windows)
2. Download installer (.dmg / .deb / .exe)
3. Double-click to install
4. Installer automatically reads browser session (no manual token copy)
5. Runner auto-registers and shows "online" in Web UI

Installer behavior:

- Bundles `astra-edge` binary
- Reads auth token from browser cookies (secure, no clipboard)
- Registers runner with auto-generated name (hostname + platform)
- Starts runner as background service (systemd / launchd / Windows Service)
- Opens browser to runner management page on completion

**Option B: CLI Registration (Developer Friendly)**

```bash
# Install astra-edge (if not already installed)
curl -sSL https://get.astra.dev/edge | sh

# Register runner
astra-edge register \
  --token rn_abc123def456 \
  --name "my-laptop" \
  --workspace ~/projects \
  --labels "dev,linux,gpu" \
  --docker-allowed true
```

Registration flow:

1. `astra-edge` validates token format locally
2. Sends `EdgeRegisterRequest` to cloud: `/api/edge/runners/register`
3. Cloud validates token, checks expiry, marks token as used
4. Cloud creates runner record in `runners` table
5. Cloud returns runner_id + long-lived runner_secret
6. `astra-edge` persists credentials to `~/.astra/edge/credentials.json`
7. `astra-edge` initiates WebSocket connection

**Option C: Hosted Runner (Zero Ops)**

For users who want cloud-managed runners without self-hosting:

1. Web UI → Add Runner → "Astra Hosted"
2. Select spec: `2C4G` / `4C8G` / `GPU (A100)`
3. Select workspace source: `git clone URL` or `upload zip`
4. Click "Create" → runner online in ~5 seconds
5. Billed per-minute of execution time

Hosted runner implementation:

- Cloud provisions VM in user's preferred region
- Auto-installs `astra-edge` and connects to user's account
- Workspace is ephemeral (deleted when runner removed)
- User can SSH into runner for debugging (optional)

### Step 3: Cloud Validation

Cloud endpoint: `POST /api/edge/runners/register`

Request:

```json
{
  "token": "rn_abc123def456",
  "name": "my-laptop",
  "platform": "linux-x86_64",
  "capabilities": {
    "shell": true,
    "docker": true,
    "gpu": false
  },
  "labels": ["dev", "linux"],
  "workspace_path": "/home/user/projects"
}
```

Validation checks:

1. Token exists and not expired
2. Token not already used
3. Token belongs to authenticated user
4. Runner name unique within workspace
5. Workspace path exists and is writable

Response:

```json
{
  "runner_id": "runner_xyz789",
  "runner_secret": "rs_secret_abc123",
  "status": "registered",
  "websocket_url": "wss://api.astra.dev/edge/ws"
}
```

### Step 4: Runner Activation

After registration, `astra-edge`:

1. Persists `runner_id` and `runner_secret` to disk
2. Connects to WebSocket endpoint with `runner_secret` in auth header
3. Sends initial heartbeat
4. Cloud updates runner status to `online`
5. Runner is now available for task routing

Heartbeat protocol:

- Interval: 30 seconds
- Payload: `{"runner_id": "...", "status": "idle", "load": 0.15, "active_tasks": 0}`
- Cloud marks runner `offline` if no heartbeat for 90 seconds
- Runner auto-reconnects on connection loss (exponential backoff: 1s → 2s → 4s → 8s → max 30s)

## User Journey

### Journey 1: Developer Registers Local Laptop

**Persona**: Software engineer, comfortable with CLI, wants to use Web Agent on private codebase.

**Web-only path** (quick setup):

1. Opens Web UI → Settings → Runners → Add Runner
2. Clicks "Quick Setup" → page shows one-click copy button
3. Copies command: `curl -sSL https://get.astra.dev/edge | sh && astra-edge register --token rn_xxx`
4. Pastes into terminal, presses Enter
5. Web UI auto-refreshes, shows runner "online" with green indicator
6. Done. Total time: 30 seconds.

**CLI path** (full control):

1. Already has `astra-edge` installed via package manager
2. Runs `astra-edge register --token rn_xxx --name laptop --labels dev,linux`
3. Runner registers, connects, shows in Web UI
4. Developer can now use Web Agent on `~/projects/private-repo`
5. Agent executes `cargo build`, `git commit`, etc. on laptop
6. Session state syncs to cloud, viewable from phone or work laptop

**Outcome**: Developer has full Web Agent capabilities on private codebase, with cloud-persisted state accessible from any device.

### Journey 2: Team Shares GPU Runner

**Persona**: ML team lead, manages shared GPU infrastructure for team.

**Setup**:

1. Team lead provisions GPU server (e.g., AWS g5.xlarge)
2. Installs `astra-edge` on server
3. Registers as team runner: `astra-edge register --token rn_xxx --name gpu-server --labels gpu,training --team-shared`
4. Configures access policy: only team members in `ml-team` group can use this runner
5. Runner shows in team's runner pool with "Shared" badge

**Team member usage**:

1. Data scientist opens Web Agent, starts session on `ml-training-repo`
2. Agent needs to run `python train.py` (requires GPU)
3. Tool router checks available runners:
   - User's laptop: no GPU capability
   - Team's `gpu-server`: has GPU, idle, user has access
4. Agent routes `bash` tool call to `gpu-server`
5. Training runs on GPU server, results stream back to Web UI
6. Session state (model checkpoints, logs) persists in cloud

**Outcome**: Team shares expensive GPU resources efficiently, with proper access control and audit trail.

### Journey 3: Ephemeral CI Runner

**Persona**: DevOps engineer, wants Web Agent to run integration tests in CI pipeline.

**Setup**:

1. CI pipeline (GitHub Actions) starts container with `astra-edge` pre-installed
2. Pipeline generates short-lived runner token via API: `astra runner token create --ttl 1h`
3. Container registers as ephemeral runner: `astra-edge register --token rn_xxx --name ci-runner-123 --ephemeral`
4. Runner connects, marked as `ephemeral` in cloud

**Usage**:

1. Web Agent session triggers integration test suite
2. Agent routes test execution to CI runner (has database access, network isolation)
3. Tests run, results stream to Web UI
4. Pipeline completes, container shuts down
5. Runner disconnects, cloud marks as `offline`
6. After 1 hour, cloud auto-deletes ephemeral runner record

**Outcome**: CI pipeline integrates with Web Agent, providing isolated test execution with full traceability.

### Journey 4: Non-Developer Uses One-Click Installer

**Persona**: Product manager, wants to use Web Agent for market research, no CLI experience.

**Setup**:

1. Opens Web UI → Settings → Runners → Add Runner
2. Selects "macOS" platform
3. Downloads `astra-edge-1.0.0.dmg` (45 MB)
4. Double-clicks DMG, drags app to Applications folder
5. Launches "Astra Edge" app
6. App shows: "Connecting to your Astra account..."
7. App reads browser session, auto-registers runner as "MacBook-Pro"
8. App shows: "Connected! Your runner is ready."
9. Web UI shows runner online with green indicator

**Usage**:

1. Product manager starts Web Agent session: "Research competitor pricing"
2. Agent uses browser automation (via MCP tool) to search web
3. Agent compiles findings into spreadsheet
4. Session state persists in cloud
5. Product manager can resume session from iPad later

**Outcome**: Non-technical user successfully uses Web Agent with zero CLI interaction.

### Journey 5: Zero-Ops Hosted Runner

**Persona**: Freelance developer, wants Web Agent but doesn't want to manage infrastructure.

**Setup**:

1. Opens Web UI → Settings → Runners → Add Runner
2. Selects "Astra Hosted" option
3. Chooses spec: `4C8G` ($0.15/hour)
4. Selects workspace: `git clone https://github.com/user/project`
5. Clicks "Create Runner"
6. Progress bar: "Provisioning VM... Installing tools... Connecting..."
7. 5 seconds later: runner online

**Usage**:

1. Developer starts session on `project` repo
2. Agent runs `npm install`, `npm test`, `git commit` on hosted runner
3. Developer can SSH into runner for debugging (optional)
4. When done, developer removes runner (or it auto-deletes after 24h idle)
5. Billed only for execution time (e.g., 2.5 hours = $0.38)

**Outcome**: Developer gets Web Agent capabilities without self-hosting, pay-per-use model.

## Runner Management Page UX

### Overview

The runner management page provides a centralized view of all runners (personal, team-shared, hosted) with clear status indicators and one-click actions.

### Runner List View

Each runner card shows:

```
┌─────────────────────────────────────────────────────────┐
│ 🟢 my-laptop                          Online · Idle     │
│ Linux x86_64 · 8 cores · 16GB RAM                      │
│ Labels: dev, linux                                      │
│ Last heartbeat: 5 seconds ago                           │
│ Active tasks: 0                                         │
│                                                         │
│ [Test Connection] [Pause] [Remove] [View History]       │
└─────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────┐
│ 🟡 gpu-server                         Online · Busy     │
│ Linux x86_64 · 16 cores · 64GB RAM · GPU: A100         │
│ Labels: gpu, training · Shared with ml-team             │
│ Last heartbeat: 2 seconds ago                           │
│ Active tasks: 1 (training job)                          │
│                                                         │
│ [Test Connection] [View Tasks] [View History]           │
└─────────────────────────────────────────────────────────┘

┌─────────────────────────────────────────────────────────┐
│ ⚫ ci-runner-123                      Offline            │
│ Linux x86_64 · Ephemeral · Auto-delete in 45 min       │
│ Labels: ci, testing                                     │
│ Last heartbeat: 3 minutes ago                           │
│                                                         │
│ [View History] [Delete Now]                             │
└─────────────────────────────────────────────────────────┘
```

Status indicators:

- 🟢 **Online · Idle**: Runner connected, no active tasks
- 🟡 **Online · Busy**: Runner connected, executing tasks
- 🔴 **Offline**: Runner disconnected (heartbeat timeout)
- ⚫ **Ephemeral**: Short-lived runner with auto-delete timer

### Actions

**Test Connection**: Sends test command to runner, verifies response within 5 seconds.

```
Test: echo "Astra connectivity test"
Result: ✅ Success (latency: 42ms)
```

**Pause**: Temporarily disables runner from receiving new tasks.

- Runner stays connected (heartbeat continues)
- Status changes to "Paused"
- Active tasks continue to completion
- User can "Resume" to re-enable

**Remove**: Permanently deletes runner.

- Confirms: "Remove runner 'my-laptop'? Active tasks will be cancelled."
- Sends disconnect signal to runner
- Deletes runner record from cloud
- Runner process exits on next heartbeat

**View History**: Shows execution history for this runner.

```
Execution History for my-laptop
─────────────────────────────────────────────────────────
2024-01-15 14:32  Session abc123  cargo build         ✅ 2m 15s
2024-01-15 14:28  Session abc123  git status          ✅ 1s
2024-01-15 13:45  Session xyz789  npm test            ✅ 45s
2024-01-15 12:10  Session xyz789  docker build        ❌ 8m 32s (timeout)
```

### Add Runner Wizard

Multi-step wizard for new runner setup:

**Step 1: Choose Type**

```
┌────────────────���────────────────────────────────────────┐
│ Add Runner                                              │
│                                                         │
│ ○ Self-Hosted Runner                                    │
│   Run on your own machine (laptop, server, VM)         │
│                                                         │
│ ○ Astra Hosted Runner                                   │
│   Cloud-managed VM, pay per use                        │
│                                                         │
│ [Next]                                                  │
└─────────────────────────────────────────────────────────┘
```

**Step 2A: Self-Hosted Setup**

```
┌─────────────────────────────────────────────────────────┐
│ Self-Hosted Runner Setup                                │
│                                                         │
│ Choose installation method:                             │
│                                                         │
│ [Quick Setup]                                           │
│ Copy and paste one command into your terminal           │
│                                                         │
│ [Download Installer]                                    │
│ Download app for macOS / Linux / Windows                │
│                                                         │
│ [Advanced (CLI)]                                        │
│ Manual installation with full configuration options     │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

**Step 2B: Hosted Runner Setup**

```
┌─────────────────────────────────────────────────────────┐
│ Hosted Runner Configuration                             │
│                                                         │
│ Machine spec:                                           │
│ ○ 2 vCPU, 4GB RAM      ($0.08/hour)                    │
│ ● 4 vCPU, 8GB RAM      ($0.15/hour)  ← selected       │
│ ○ 8 vCPU, 16GB RAM     ($0.30/hour)                    │
│ ○ GPU: A100, 40GB      ($2.50/hour)                    │
│                                                         │
│ Workspace source:                                       │
│ ○ Git repository URL: [________________]                │
│ ○ Upload zip file: [Choose File]                        │
│                                                         │
│ Region: [US East (Virginia) ▼]                          │
│                                                         │
│ [Create Runner]                                         │
└─────────────────────────────────────────────────────────┘
```

**Step 3: Confirmation**

```
┌─────────────────────────────────────────────────────────┐
│ ✅ Runner Created Successfully!                         │
│                                                         │
│ Name: my-laptop                                         │
│ Status: Connecting...                                   │
│                                                         │
│ Your runner will appear in the list once connected.     │
│                                                         │
│ [View Runner] [Done]                                    │
└─────────────────────────────────────────────────────────┘
```

### Accessibility & UX Best Practices

- **Status indicators**: Use color + text + icon (not color alone) for accessibility
- **Keyboard navigation**: All actions accessible via Tab + Enter
- **Screen reader support**: ARIA labels for status indicators and actions
- **Mobile responsive**: Runner cards stack vertically on small screens
- **Real-time updates**: WebSocket pushes status changes instantly (no polling)
- **Error states**: Clear error messages with retry actions
  - "Runner offline: Check network connection and restart astra-edge"
  - "Test failed: Runner did not respond within 5 seconds"
- **Tooltips**: Hover over labels/icons for explanations
- **Search & filter**: Filter runners by status, labels, team
- **Bulk actions**: Select multiple runners for bulk pause/remove

## Usage Journeys

The following journeys illustrate how different users interact with Astra Web Agent across devices, roles, and time. Each journey is written from the user's perspective — no internal implementation details.

### Journey A: Solo Developer — Persistent Workspace

**Scenario:** A developer works on a feature over multiple days in the same workspace.

**Day 1 — Setup**

1. Open Astra Web UI, click "New Project"
2. Paste a Git repo URL (or select from recent repositories)
3. Select runner: "My Laptop" (already registered)
4. Astra clones the repo into a new workspace `ws-api-service`
5. Tell agent: "Run the tests to make sure everything works"
6. Agent detects the project language and toolchain, runs the test suite, reports all passing ✅
7. Tell agent: "Add rate limiting to the file upload endpoint"
8. Agent reads existing code, creates a branch, starts implementing

**Day 2 — Continue from phone on commute**

1. Open Astra Web UI on phone — session is still there, exactly where it left off
2. See the branch diff, review what the agent wrote yesterday
3. Tell agent: "Add unit tests for the rate limiter"
4. Watch agent write tests, all pass
5. Close phone — work is saved automatically

**Day 3 — Finish on laptop**

1. Open Astra Web UI on laptop — same session, same workspace
2. Workspace still has all files, build cache, dependencies — nothing re-downloaded
3. Tell agent: "Create a PR"
4. Agent pushes branch, opens PR with description and test results
5. Reviewer comments on PR — agent auto-addresses feedback
6. PR merged — workspace retains history for next task

**Key behavior:** The workspace persists across sessions. Files, build artifacts, dependency caches, git branches, and conversation memory all accumulate. Opening a new session in the same workspace feels like reopening your IDE — everything is where you left it.

---

### Journey B: Team Collaboration — Shared Workspace

**Scenario:** Two developers and a designer collaborate on a UI feature.

1. **Developer A** creates session "Dashboard redesign" in shared workspace `ws-dashboard`
2. **Developer A** asks agent to refactor the chart components
3. **Designer** opens the same session from their browser — sees the full trace of what was done
4. **Designer** tells agent: "Change the color scheme to match our design system tokens"
5. Agent understands the context because the session state is cloud-only — it has full history from Developer A's work
6. **Developer B** forks the session to explore a different approach: "What if we use a different visualization library?"
7. Fork creates a parallel workspace branch — Developer B experiments without affecting the main workspace
8. Developer B likes the result → merges fork back to main workspace
9. **Developer A** resumes main session, sees the D3 changes, approves

**Key behavior:** Cloud-only state means every collaborator sees the exact same session view regardless of device. Fork creates an isolated sandbox that can be merged back. No one steps on anyone else's toes.

---

### Journey C: Async Handoff — Timezone-Friendly Workflow

**Scenario:** A developer in Berlin starts work, an agent continues overnight, a reviewer in San Francisco picks it up.

1. **Berlin dev** creates session, tells agent to implement a feature
2. **Berlin dev** goes offline at 6 PM — agent continues working on the runner
3. Next morning, **Berlin dev** opens Web UI — agent finished, left a summary:
   - "Implemented X, tested Y, found issue Z — fixed it"
   - Full trace available: every decision, every command, every file change
4. **SF reviewer** opens the same session link — reviews agent's work asynchronously
5. **SF reviewer** leaves comments → agent addresses them on the runner
6. **Berlin dev** wakes up to a resolved PR

**Key behavior:** Sessions are async-first. The agent works independently on the runner; humans review and steer when they're available. The cloud state bridges timezones seamlessly.

---

### Journey D: Product Manager — Research & Analysis

**Scenario:** A product manager needs competitive analysis, no coding required.

1. Open Astra Web UI
2. Click "New Session" → select "Astra Hosted" runner (no setup needed)
3. Tell agent: "Analyze pricing strategies of 5 competitors in the project management space"
4. Agent uses web search tools to gather data, organizes findings into a structured report
5. PM says: "Put it in a comparison table with monthly/annual pricing tiers"
6. Agent generates a formatted table
7. PM says: "Export as PDF and share with the team"
8. Agent produces PDF artifact, generates shareable link
9. Team members open the link — see the full research trace, can ask follow-up questions

**Key behavior:** Non-technical users get the same agent power without touching terminals or runners. Hosted runners eliminate setup friction entirely.

---

### Journey E: Operations — Data Processing

**Scenario:** An ops analyst needs to clean and transform a large dataset.

1. Upload Excel file (50MB, 200K rows) via Web UI
2. Select Astra Hosted runner
3. Tell agent: "Remove duplicates, standardize date formats, fill missing city names from postal codes"
4. Agent writes and executes a Python script on the runner
5. Ops analyst watches progress: "Processing row 50,000 / 200,000..."
6. Agent completes, provides download link for cleaned file
7. Ops analyst forks session: "Now generate a pivot table by region"
8. Forked session inherits the cleaned data — no re-processing needed

**Key behavior:** Large data processing works because the runner has real compute resources. Forking avoids redundant work.

---

### Journey F: Designer — Batch Asset Processing

**Scenario:** A designer needs to process hundreds of images.

1. Register Mac Studio as a runner (has Adobe tools installed)
2. Create session "Product photo batch processing"
3. Bind to workspace with the raw photos directory
4. Tell agent: "Crop all images to 1:1 ratio, add white background, export as PNG"
5. Agent uses ImageMagick commands on the runner
6. Check progress from iPad while at a meeting
7. Agent finishes — designer reviews output quality in Web UI
8. "Good, but the shadows look off on 10 images" → agent fixes just those

**Key behavior:** The runner gives access to the designer's local tools and hardware. Cross-device monitoring lets them check progress from anywhere.

---

### Journey G: Multi-Runner Pipeline

**Scenario:** A ML engineer needs a pipeline spanning multiple environments.

1. Register three runners:
   - `dev-laptop`: for code editing and testing
   - `gpu-server`: for model training (A100 GPU)
   - `staging-cluster`: for integration testing

2. Create session "Train recommendation model v3"
3. Develop preprocessing code on `dev-laptop` runner
4. Switch execution to `gpu-server` for training:
   - "Train the model on the production dataset using the GPU server"
   - Agent detects capability requirement, routes to GPU runner
5. Training complete → switch to `staging-cluster`:
   - "Deploy the trained model to staging and run load tests"
6. All three runners' outputs visible in the same session timeline
7. Full audit trail: which code ran on which machine, what outputs were produced

**Key behavior:** Multi-runner orchestration lets agents span heterogeneous infrastructure while maintaining a single coherent session view.

---

### Journey H: Knowledge Accumulation

**Scenario:** A senior developer builds institutional knowledge in Astra over months.

1. Over 6 months, developer works on the same workspace through dozens of sessions
2. Astra accumulates:
   - Which patterns the team prefers
   - Common debugging steps for this codebase
   - Build configurations that work on this runner
   - Test coverage gaps that keep coming up
3. New developer joins the team
4. Opens a session in the same workspace
5. Tells agent: "Help me understand the authentication flow"
6. Agent draws on accumulated traces, plans, and decisions from previous sessions
7. Provides a guided walkthrough with links to relevant historical sessions
8. New developer becomes productive in hours instead of weeks

**Key behavior:** Cloud-only state with long-term memory turns Astra from a tool into a living knowledge base. The workspace is not just files — it's the team's collective experience encoded in traces and plans.

## Workspace Model

A workspace is **not a directory**. It is a compound entity that accumulates five classes of state across sessions. Each class has distinct persistence, scope, and lifecycle rules.

### The Five Components

```
┌─────────────────────────────────────────────────────────┐
│                     WORKSPACE                           │
│                                                         │
│  ┌──────────┐  ┌──────────┐  ┌──────────────────────┐  │
│  │   Code   │  │   Data   │  │      Memory          │  │
│  │          │  │          │  │                      │  │
│  │ repos    │  │ datasets │  │ session history      │  │
│  │ branches │  │ fixtures │  │ decisions made       │  │
│  │ commits  │  │ snapshots│  │ patterns learned     │  │
│  │ working  │  │ env data │  │ preferences          │  │
│  │   tree   │  │          │  │                      │  │
│  └──────────┘  └──────────┘  └──────────────────────┘  │
│                                                         │
│  ┌──────────────────┐  ┌────────────────────────────┐   │
│  │    Knowledge     │  │        Artifacts           │   │
│  │                  │  │                            │   │
│  │ documentation    │  │ build outputs (target/,    │   │
│  │ design docs      │  │   dist/, binaries)         │   │
│  │ wikis / runbooks │  │ Docker images              │   │
│  │ specifications   │  │ cached deps (node_modules, │   │
│  │ team conventions │  │   venv, Cargo registry)    │   │
│  │                  │  │ generated reports, PDFs    │   │
│  └──────────────────┘  └────────────────────────────┘   │
└─────────────────────────────────────────────────────────┘
```

#### 1. Code

The working tree — source files, git history, branches.

- **Persistence**: On runner (disk). Survives runner restarts.
- **Scope**: Per project within workspace.
- **Lifecycle**: Created via clone/init; updated via agent edits, git pull, branch switch; archived with workspace.
- **Isolation**: Multiple sessions operate on different branches. Same-branch concurrent edits blocked by file lock.

#### 2. Data

Structured information the agent reads, transforms, or generates.

- **Persistence**: On runner (disk). May be gitignored.
- **Scope**: Shared across sessions in the same workspace.
- **Lifecycle**: Seeded by user (upload, clone), enriched by agent (generated test data, scraped results).
- **Examples**: CSV datasets, JSON fixtures, DB snapshots, API response caches, test corpora.

Data differs from Code: Code is version-controlled source; Data is the material the code operates on.

#### 3. Memory

The agent's accumulated experiential state — what it learned, decided, and prefers.

- **Persistence**: Cloud (synced). Survives runner destruction.
- **Scope**: Per workspace. Sessions within the same workspace share memory context.
- **Lifecycle**: Grows organically with each session. Agent recalls past decisions, avoids repeating mistakes, remembers project conventions.
- **Examples**:
  - "This project uses snake_case for Rust, camelCase for TypeScript"
  - "The OAuth module in this workspace has a known quirk: tokens must be refreshed 5 min before expiry"
  - "Last 3 sessions all failed at `make test-integration` due to missing Docker — agent proactively checks Docker availability now"

Memory is the key differentiator from ephemeral cloud sandboxes. A workspace with 50 sessions of accumulated memory makes the agent qualitatively better at working in that environment.

#### 4. Knowledge

Structured reference material the agent indexes and retrieves.

- **Persistence**: On runner (files) + Cloud (index).
- **Scope**: Workspace-level (shared across all projects).
- **Lifecycle**: Curated by user (add docs, wikis, specs), auto-discovered by agent (README, CONTRIBUTING.md, design docs).
- **Examples**: API specs, architecture decision records (ADRs), runbooks, team style guides, dependency docs.

Knowledge differs from Memory: Knowledge is explicit reference material (documents); Memory is implicit experiential state (what the agent learned by doing).

#### 5. Artifacts

Material outputs of agent work — build products, cached dependencies, generated files.

- **Persistence**: On runner (disk). Some may be uploaded to cloud for sharing.
- **Scope**: Per project.
- **Lifecycle**: Created/destroyed by agent actions. Should be reproducible (not the primary store of value — Code and Memory are).
- **Examples**: `target/`, `dist/`, binaries, Docker images, `node_modules/`, compiled WASM, PDF reports, screenshots.

### State Accumulation Table

| Component     | Persistence    | Scope     | Lost if runner dies?              | Transferable to new runner? |
| ------------- | -------------- | --------- | --------------------------------- | --------------------------- |
| **Code**      | Runner disk    | Project   | ❌ Yes (must backup)              | Clone from remote git       |
| **Data**      | Runner disk    | Workspace | ❌ Yes                            | Upload / re-generate        |
| **Memory**    | Cloud          | Workspace | ✅ No                             | ✅ Instant (cloud-native)   |
| **Knowledge** | Runner + Cloud | Workspace | ⚠️ Partial (cloud index survives) | ✅ Cloud index transfers    |
| **Artifacts** | Runner disk    | Project   | ❌ Yes                            | Rebuild from Code           |

### Workspace Affinity

Workspace Affinity describes the **gravitational relationships** between a workspace and other entities in the Astra system. The stronger the affinity, the less friction in routing work to that workspace.

#### Affinity Map

```
                    ┌──────────────┐
       owns ────────│   User/Team  │──────── owns
       │            └──────────────┘            │
       ▼                         │              ▼
┌──────────┐                     │       ┌──────────┐
│ Runner A │◄──── hosts ──── WORKSPACE ── hosts ──►│ Runner B │
│ (dev)    │                     │       │  (gpu)   │
└──────────┘                     │       └──────────┘
                                 │
                    ┌────────────┼────────────┐
                    ▼            ▼            ▼
              ┌──────────┐ ┌──────────┐ ┌──────────┐
              │ Project X│ │ Project Y│ │ Project Z│
              │ (Rust)   │ │ (Python) │ │ (Docs)   │
              └──────────┘ └──────────┘ └──────────┘
```

#### Affinity Rules

| Rule                        | Description                                                                                                                                                        | Example                                                                                                                                 |
| --------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------- |
| **Runner Affinity**         | A workspace lives on one primary runner. All sessions targeting this workspace are routed there. Only one runner can hold the canonical workspace state at a time. | `my-laptop-runner` hosts `astra-dev` workspace. All sessions using `astra-dev` execute on `my-laptop`.                                  |
| **User/Team Affinity**      | A workspace is owned by a user or team. Ownership controls access, sharing, and billing.                                                                           | Team "Platform" owns `platform-monorepo` workspace. All team members can create sessions in it.                                         |
| **Project Affinity**        | A workspace contains N projects. Each project maps to a git repo (or monorepo path). Sessions bind to a specific project within the workspace.                     | `astra-dev` workspace contains projects `astra-core`, `astra-web`, `astra-edge`.                                                        |
| **Task Affinity** (soft)    | Certain task types naturally perform better on certain workspaces due to accumulated state. The scheduler _prefers_ matching workspaces but does not enforce.      | Test suite runs 50x faster on `dev-workspace` (cached deps) than on a fresh hosted runner. Agent prefers `dev-workspace` for test runs. |
| **Session Affinity** (soft) | A session remembers its workspace. Resumed sessions reconnect to the same workspace. Forked sessions can choose a different workspace.                             | Session "fix-oauth" was created in `astra-dev`. Resume opens the same workspace.                                                        |

#### Affinity in Practice

**Forking a session**:

```
Original session: "add-search" in workspace "team-frontend" on runner "office-mac"
                                                            │
                                              Fork (user picks new workspace)
                                                            │
                                      ┌─────────────────────┼─────────────────────┐
                                      ▼                     ▼                     ▼
                              Same workspace         New workspace         Hosted runner
                              (team-frontend)        (my-laptop)           (astra-hosted-us)
                              │                      │                     │
                       Fastest — all deps     Fresh clone needed     Fresh clone needed
                       cached, code there     deps install from      deps install from
                                              scratch                scratch
```

The system should **surface affinity costs** to the user: "Forking to a new workspace will require a fresh build (~8 min). Fork in the current workspace for instant start."

#### Workspace Relocation

A workspace can be **relocated** from one runner to another:

```
WORKSPACE RELOCATION

Runner A (old)                              Runner B (new)
┌─────────────────────┐                    ┌─────────────────────┐
│ Workspace tarball   │ ──── transfer ───► │ Workspace restored  │
│ Code + Data +       │                    │ + Memory (cloud)    │
│ Artifacts +         │                    │ + Knowledge (cloud) │
│ Knowledge files     │                    │                     │
└─────────────────────┘                    └─────────────────────┘
```

1. Runner A creates a tarball of workspace files (Code + Data + Artifacts + Knowledge docs)
2. Tarball transferred to Runner B (direct or via cloud intermediary)
3. Runner B restores tarball
4. Cloud re-links Memory and Knowledge index to Runner B
5. Old workspace on Runner A is archived (not deleted — safety net)

**Use cases**:

- Upgrade laptop, move workspace from old Mac to new Mac
- Move from local laptop to a team GPU server
- Onboard new team member: clone team workspace to their runner

### Project Lifecycle

A **project** is a logical unit within a workspace, corresponding to a git repository or monorepo directory:

1. **Create**: User creates project in workspace via Web UI or CLI
2. **Clone**: Runner clones the repo (or user uploads code)
3. **Bootstrap**: Agent runs setup commands (install deps, build, test) and records conventions in Memory
4. **Accumulate**: Each session adds Code changes, Memory, and Artifacts
5. **Update**: Agent pulls latest changes, switches branches, merges
6. **Archive**: Project archived (tarball snapshot). Can be restored later.

### Session-to-Workspace Binding

When creating a session in the Web UI:

```
Create Session
├── Name: "Fix OAuth token refresh"
├── Workspace: "astra-dev"          ← persistent, accumulates state
├── Project: "astra-core"           ← git repo within workspace
├── Branch: "fix-oauth-refresh"     ← started from main
└── Runner: auto (affinity → my-laptop)
```

The session's Memory and trace are cloud-native. All file operations execute in the selected workspace on the affinity runner.

### Multi-Session Isolation

Multiple sessions can operate on the same workspace concurrently:

- **Branch isolation**: Each session works on a different branch (default)
- **Same-branch safety**: If two sessions target the same branch, the runner enforces file-level locking — second session waits or forks
- **Conflict resolution**: If session A pushes to branch while session B has uncommitted changes, session B must rebase before pushing

### Workspace Lifecycle Commands

```bash
# Create workspace on a runner
astra workspace create --name "my-project" --runner "my-laptop"

# List workspaces (with runner affinity shown)
astra workspace list
#  NAME           RUNNER        PROJECTS  LAST ACTIVE
#  astra-dev      my-laptop     3         2 min ago
#  team-frontend  office-mac    1         3 days ago

# Add project (clone repo into workspace)
astra workspace add-project --workspace "my-project" \
  --git-url "https://github.com/org/repo" --branch "main"

# Switch project branch
astra workspace switch --workspace "my-project" \
  --project "repo" --branch "feature-x"

# Relocate workspace to another runner
astra workspace relocate --workspace "my-project" \
  --from-runner "old-laptop" --to-runner "new-laptop"

# View workspace state
astra workspace status --name "my-project"
#  Code:      repo@main (clean)
#  Memory:    142 sessions, 3.2K learnings
#  Artifacts: 847 MB (build cache), 2.1 GB (deps)
#  Knowledge: 23 docs indexed

# Archive workspace (tarball + freeze)
astra workspace archive --name "my-project"

# Restore archived workspace to a runner
astra workspace restore --name "my-project" --runner "new-laptop"
```

### Web UI Workspace Dashboard

The Web UI provides visual workspace management:

- **Workspace list**: All workspaces with runner, project count, last active, total session count
- **Project browser**: File tree with syntax highlighting, branch selector, git log
- **Memory explorer**: Browse accumulated learnings — what the agent knows about this workspace
- **Knowledge index**: List of indexed documents (specs, ADRs, runbooks) with search
- **Artifact view**: Cached deps size, build outputs, generated files
- **Terminal**: Web-based terminal connected to the workspace on the runner
- **Relocation wizard**: Step-by-step UI to move a workspace to a new runner

### User Journey: Long-Running Development

**Scenario**: Developer builds an OAuth feature over 3 weeks.

_Week 1, Monday — Bootstrap_:

1. Web UI → Create workspace "astra-dev" on runner "my-laptop"
2. Add project "astra-core" (clone from GitHub)
3. Session "setup": agent installs toolchain, runs `cargo build`, runs tests
4. Session ends. Workspace now has: **Code** (repo), **Artifacts** (compiled deps, 2.1 GB cache), **Memory** (1 learning: "Rust toolchain is stable, all 847 tests pass")

_Week 1, Wednesday — Development_:

1. Session "impl-oauth": same workspace, instant start (no re-install)
2. Agent creates branch, writes 500 lines, runs incremental build (fast, deps cached)
3. Agent commits, pushes branch
4. Workspace now has: **Code** (new branch + 500 lines), **Memory** (23 new learnings about OAuth flow patterns), **Artifacts** (incremental build cache updated)

_Week 2, Monday — Cross-device debugging_:

1. Developer on phone → opens same workspace in Web UI
2. Session "fix-oauth-bug": agent checks out feature branch, finds bug, fixes 20 lines
3. Agent runs tests (still fast, same cache), commits
4. Workspace now has: **Code** (bug fix), **Memory** (1 learning: "token refresh must happen 5 min before expiry")

_Week 3, Friday — Delivery_:

1. Session "create-pr": agent creates PR with summary, pushes final branch
2. Workspace now has: **Memory** (mark: "OAuth feature complete, PR #3421, 3-week effort"), **Knowledge** (PR description saved as reference)

_Week 4 — Onboarding_:

1. New team member joins, clones workspace "astra-dev" to their runner
2. All **Memory** transfers instantly (cloud-native)
3. New member's first session: agent already knows project conventions, test patterns, OAuth module quirks
4. Time-to-productivity: hours, not days

**Key behavior**: The workspace is a living, accumulating entity. Code is the most visible component, but Memory and Knowledge compound over time to make the agent qualitatively better at working in _this specific environment_.

## Coding Workflows

A runner with a persistent workspace transforms how developers work with Astra. Instead of ephemeral sessions that start from scratch, the workspace accumulates code, context, and tooling over time. Below are all the usage modes and workflows for the coding scenario.

### 1. Workspace Bootstrapping

The first time a developer sets up a project:

```
Web UI → Create Workspace → "astra-core"
├── Runner: "my-laptop"
├── Clone: git@github.com:org/astra.git (branch: main)
├── Setup script: `make setup && cargo build && cargo test`
└── Result: Workspace ready. 2GB dependencies cached. Build green.
```

**Key behavior**: The setup script runs once. All subsequent sessions inherit the environment — no reinstalling dependencies, no recompiling from scratch.

### 2. Daily Development Loop

The most common workflow — open a session, do work, close:

```
Morning:
├── Open Web UI → Session "Implement feature X"
├── Workspace: "astra-core" (branch: main)
├── Tell agent: "Implement rate limiting for the API endpoints"
├── Agent on runner:
│   ├── Reads existing code (understands project patterns from prior sessions)
│   ├── Creates branch: `feature/rate-limit`
│   ├── Writes code (follows project conventions learned from session memory)
│   ├── Runs `cargo test` (incremental build: 12s, not 3min)
│   └── All tests pass
└── Session ends. Branch persists on runner.

Afternoon:
├── Open Web UI → Session "Add rate limit tests"
├── Workspace: "astra-core" (branch: feature/rate-limit, same as morning)
├── Tell agent: "Add edge case tests for the rate limiter"
├── Agent knows the context (session memory from morning is available)
├── Writes tests, runs them, all pass
└── Session ends.
```

**Key behavior**: The workspace remembers where you left off. Branch, code, build cache — all persist. The agent remembers what it did.

### 3. Debugging and Investigation

When something breaks:

```
├── Open Web UI → Session "Debug: API timeout in production"
├── Workspace: "astra-core" (branch: main)
├── Tell agent: "Production is timing out on /api/users, investigate"
├── Agent on runner:
│   ├── Reads recent git log (workspace has full history)
│   ├── Identifies suspect commit from 3 days ago
│   ├── Writes reproduction test
│   ├── Runs test — confirms the bug
│   ├── Fixes the bug (4 lines changed)
│   ├── Runs full test suite (incremental: 45s)
│   └── All tests pass
└── Session ends. Fix is on branch `fix/api-timeout`.
```

**Key behavior**: The agent leverages accumulated git history, build cache, and prior session knowledge to debug efficiently. No cold start.

### 4. Branch and PR Workflow

The complete feature lifecycle:

```
Day 1: Create feature branch
├── Session "Start: user dashboard"
├── Agent: creates `feature/dashboard`, scaffolds components

Day 2: Continue development
├── Session "Continue: dashboard components"
├── Agent: resumes on `feature/dashboard` (branch persisted)
├── Writes React components, runs tests

Day 3: Review and polish
├── Session "Review: dashboard PR"
├── Agent: self-reviews the code, fixes linting issues
├── Runs `cargo clippy`, `cargo fmt`
├── Pushes branch, creates PR via GitHub API

Day 4: Address review comments
├── Session "Address PR review"
├── Agent: reads reviewer comments on GitHub
├── Makes requested changes on `feature/dashboard`
├── Pushes updates
```

**Key behavior**: Each session picks up exactly where the previous one left off. The branch, the code, the context — all continuous.

### 5. Multi-Device Continuity

Start on one device, continue on another:

```
Office (Desktop):
├── Session "Refactor auth module"
├── Agent on runner "office-server": refactors 2000 lines
├── Tests pass. Session paused (not finished).

Commute (Phone):
├── Open Web UI on phone
├── Same session "Refactor auth module" — sees full trace
├── Reviews what the agent did, reads the diff
├── Tells agent: "Also update the integration tests"
├── Agent on runner "office-server": updates tests
├── Watches test results stream in real-time on phone

Home (Laptop):
├── Same session, same workspace
├── Tells agent: "Create a PR with a detailed description"
├── Agent creates PR
└── Done.
```

**Key behavior**: The runner is the persistent execution surface. Any device with a browser can observe and direct the work. State is always consistent.

### 6. Parallel Feature Development

Multiple sessions, same workspace, different branches:

```
├── Session A: "Feature: notifications" → branch `feature/notifications`
├── Session B: "Feature: export CSV"    → branch `feature/export-csv`
├── Session C: "Bugfix: login redirect" → branch `fix/login-redirect`
│
├── Each session works on its own branch
├── Build caches are shared (cargo registry, npm cache)
├── Agent in Session A doesn't interfere with Session B
├── If Session A merges first, Session B agent detects and rebases
└── All three PRs created from the same workspace
```

**Key behavior**: Branch isolation within a shared workspace. Dependencies and build caches are reused, making parallel work efficient.

### 7. Long-Running Tasks

Tasks that take hours — overnight builds, batch testing, data migration:

```
Evening (6 PM):
├── Session "Run full integration test suite"
├── Tell agent: "Run all 2000 integration tests, report failures"
├── Agent on runner: starts `cargo test --all --release`
├── Close browser. Go home.

Overnight:
├── Agent continues running tests on runner
├── 47 tests fail. Agent analyzes each failure.
├── Agent categorizes: 12 are flaky, 35 are real bugs.
├── Agent creates a summary report in the session trace.

Morning (9 AM):
├── Open Web UI on phone
├── Session "Run full integration test suite" — completed
├── Read summary: "35 real failures, 12 flaky. Top 5 failures: ..."
├── New session "Fix top 5 test failures"
├── Agent on runner: fixes them one by one
└── Done by lunch.
```

**Key behavior**: The runner executes independently of the browser. The agent works autonomously and reports back. The developer reviews results and directs next steps.

### 8. Collaborative Development

Two developers sharing a workspace:

```
Developer A (Shanghai):
├── Session "Implement API v2" → branch `feature/api-v2`
├── Agent writes API handlers, runs tests
├── End of day: pushes branch, session paused

Developer B (San Francisco):
├── Opens same workspace "astra-core"
├── Creates session "Review: API v2"
├── Agent on runner reads Developer A's branch
├── Agent reviews code, finds 3 issues
├── Developer B tells agent: "Fix the issues and push"
├── Agent fixes, pushes, comments on PR

Next morning, Developer A:
├── Opens session "Continue: API v2"
├── Sees the fixes from Developer B's session
├── Tells agent: "Good fixes. Add missing docs."
├── Agent adds docs, pushes
└── PR ready for merge.
```

**Key behavior**: Shared workspace + cloud session state = seamless async collaboration. Each developer's agent interactions are in separate sessions but operate on the same codebase.

### 9. Monorepo and Multi-Project Work

Workspace with multiple projects:

```
Workspace: "platform"
├── Project A: "frontend" (Next.js, branch: main)
├── Project B: "backend" (Rust, branch: main)
├── Project C: "shared-types" (TypeScript, branch: main)

Session: "Add user preferences feature"
├── Tell agent: "Add user preferences across frontend and backend"
├── Agent on runner:
│   ├── Modifies Project C: adds `UserPreferences` type
│   ├── Modifies Project B: adds API endpoints
│   ├── Modifies Project A: adds settings page
│   ├── Runs `cargo test` in backend
│   ├── Runs `npm test` in frontend
│   └── All tests pass
└── Three coordinated changes, one session.
```

**Key behavior**: The agent understands project boundaries and can make coordinated changes across multiple projects in a monorepo.

### 10. Environment-Specific Work

Different runners for different environments:

```
Runner "dev-laptop" (macOS, ARM):
├── Workspace "astra-core"
├── Session: "Develop and test locally"
├── Agent develops feature, runs tests on macOS

Runner "linux-server" (Linux, x86_64, 32 cores):
├── Same workspace "astra-core" (synced via git)
├── Session: "Run full CI suite"
├── Agent pulls latest, runs exhaustive tests on Linux

Runner "gpu-server" (Linux, NVIDIA A100):
├── Workspace "ml-models"
├── Session: "Train model v2"
├── Agent runs training script on GPU
```

**Key behavior**: The developer directs work to the appropriate runner based on the task. Lightweight development on a laptop, heavy computation on a server.

### Workflow Summary

| Mode                  | Session Count | Duration          | Key Feature                            |
| --------------------- | ------------- | ----------------- | -------------------------------------- |
| **Bootstrapping**     | 1             | 10 min            | One-time setup, dependencies cached    |
| **Daily Loop**        | 1-2/day       | 30 min each       | Incremental builds, context carry-over |
| **Debugging**         | 1             | 1-2 hours         | Git history access, reproduction tests |
| **Branch/PR**         | 3-5 over days | Days              | Persistent branch, continuous context  |
| **Multi-Device**      | 1             | Hours             | Any device observes/directs same work  |
| **Parallel Features** | 3+ concurrent | Ongoing           | Branch isolation, shared caches        |
| **Long-Running**      | 1             | Hours (overnight) | Runner executes without browser        |
| **Collaborative**     | 2+ per person | Days              | Shared workspace, async handoff        |
| **Monorepo**          | 1             | Hours             | Cross-project coordinated changes      |
| **Multi-Runner**      | 1 per runner  | Varies            | Task-directed runner selection         |

## Anti-Patterns

- Do not store runner task state only in process memory. All tasks must be persisted in `runner_tasks` for audit and resume.
- Do not allow runners to authenticate as other users. Runner tokens are scoped to the registering user and workspace.
- Do not bypass sandbox for "trusted" runners. All execution must be sandboxed, even for admin users.
- Do not stream unredacted secrets back to cloud. Runner output must be filtered for credentials before storage.
- Do not infer runner health from task success. Heartbeat is the only authoritative health signal.
- Do not allow peer-to-peer runner communication. All coordination flows through the cloud to maintain auditability and security.
