# Runtime tool boundary

> Status: target design contract.
> Last updated: 2026-09-03.

Runtime tool boundary defines what may execute in which authority domain. It is narrower than provider routing: routing decides where a tool goes; this boundary defines what each runtime is allowed to do once selected.

## Ownership

This document owns:

- local executor authority boundaries;
- workspace root and path authority;
- shell/file/git side-effect boundaries;
- server-safe runtime constraints;
- sandbox separation;
- result containment and redaction boundary.

It does not own:

- provider priority and fallback, owned by [capability-system.md](capability-system.md);
- Edge/cloud composition, owned by [edge-cloud-execution.md](edge-cloud-execution.md);
- Web surface behavior, owned by [web-agent-runner.md](web-agent-runner.md);
- permission policy, owned by [safety-and-permissions.md](safety-and-permissions.md).

## Runtime domains

| Domain | Authority |
| --- | --- |
| Server control plane | Astra cloud state, metadata, run/session/task/artifact operations. |
| Server-safe runtime | Explicitly configured cloud-safe tools such as configured `web_fetch` or sandboxed service tools. |
| Edge/CLI local runtime | User-owned workspace, local shell, file, git, local network, local MCP. |
| Request-scoped MCP | Explicit remote API operations under request/session binding. |
| Cloud workspace runtime | Externally provisioned workspace runtime with declared isolation and transport. |

## Server boundary

The server default runtime must not imply:

- arbitrary host shell;
- arbitrary host file read/write;
- git mutation on user workspace;
- user-local browser/session identity;
- private network access;
- local MCP server access.

Server may expose a tool only when a provider contract explicitly grants that capability and safety policy permits it.

## Edge boundary

Edge/CLI local runtime may expose local capabilities only within declared authority:

- selected workspace root;
- permission policy;
- sandbox mode;
- user identity and local environment;
- provider health and binding lifetime.

Path authority must be checked before execution. A model-provided path is not authority by itself.

## Cloud workspace boundary

A cloud workspace runtime is not the same as the Astra server process. It requires explicit provider binding and isolation metadata.

Required metadata:

```text
workspace_id
runtime_id
provider_id
transport
isolation_backend
workspace_root
authority
fallback_policy
```

## Side-effect boundary

Side-effecting tools require stronger checks than read-only tools.

| Side effect | Requirement |
| --- | --- |
| File write | workspace authority and mutation permission. |
| Shell command | executor provider, sandbox/policy approval, timeout. |
| Git mutation | workspace authority, user policy, clear target repo. |
| External mutation | explicit API authority and approval policy. |
| Credential access | deny by default unless dedicated secret provider authorizes scoped access. |

### Workspace observation coordination

Local shell and typed workspace writers must serialize each executor-owned
pre/execute/post observation window across Astra processes without placing the
coordination authority inside the tool-writable workspace.

- Linux uses a root-owned sticky temporary root, a kernel-owned abstract Unix
  socket name for each workspace generation, a per-UID integrity witness, and
  inotify-backed sticky tamper evidence.
- macOS uses the root-owned sticky `/private/tmp` root, a deterministic OFD
  record-lock byte on that protected directory inode for each workspace
  generation, a per-UID integrity witness, and kqueue vnode-backed sticky
  tamper evidence. Witness watches retain the admitted file description;
  opened binding descriptors must match the captured device, inode, and file
  type. Event polling rechecks permanent revocation under its mutex so a
  concurrent reader cannot accept an already-revoked generation.
  A contender first reserves its byte with a shared OFD lock,
  then probes for any other description through a hypothetical exclusive lock.
  Concurrent contenders can retreat together, but cannot both be admitted;
  process-diverse jitter restores progress without a machine-global admission
  gate.
- Kernel ownership must end automatically when the holder process exits.
  Replacing or unlinking a witness or workspace binding must not admit a second
  generation, and any observed tamper revokes receipt authority permanently
  for the active lease.
- Platforms without an equivalent trusted namespace and tamper watch fail
  closed before launching a local command.

Receipt attribution and execution coordination are independent. A completed
foreground process group may be too weak to authorize future fingerprint-based
receipts: an escaped descendant could write later. That uncertainty quarantines
receipt attribution, but does not by itself revoke a healthy coordination lease
or prevent the next command. Unsettled execution ownership and a replaced or
tampered binding still prevent admission. Every receipt-producing path must
check receipt authority independently of the coordination check used to launch.

### Prepared directory inspection

Prepared Unix Bash invocations inspect targets through the same retained
workspace and working-directory handles used for execution. Directory acquisition
walks each component without following symlinks. Target inspection resolves
symlinks with bounded traversal and checks directory ancestry by identity; it
does not assume that an existing `/dev/fd` entry supports child-path lookup.
The OS sandbox remains responsible for execution-time filesystem confinement.

Source preimages read retained regular-file handles with a bounded byte budget.
They retain the captured parent identity in the receipt: replacing a parent must
not turn another file into a valid post-image or restore destination. Restore
checks the workspace binding and opens targets relative to retained directories.
Working-directory names are verified display labels, not IO authority; an
unavailable label is reported as unknown rather than silently as the root.

## Result boundary

Tool output crossing runtime boundaries must be enveloped:

```text
tool_call_id
provider_id
runtime_domain
status
visible_summary
raw_artifact_ref
quality_status
redaction_status
error_kind
```

Raw bytes should not be blindly streamed into UI, prompt, trace, or learning data.

## Network and proxy boundary

Edge network access remains inside the selected provider's authority. When an
outbound proxy is configured, the runtime must:

- honor explicit proxy-bypass rules before connecting;
- keep proxy credentials out of logs, traces, errors, and persisted runtime
  configuration;
- bound connect, tunnel negotiation, and protocol-upgrade time;
- reject unsupported proxy schemes instead of silently weakening transport
  security.

Proxy routing does not expand workspace, identity, or private-network
authority; it only changes transport for an already-admitted operation.

## Required invariants

- Server default capacity cannot execute local workspace tools.
- Edge authority cannot escape selected workspace without explicit approval.
- MCP API tools cannot impersonate local executor tools.
- Cloud workspace runtime cannot be assumed online from workspace existence.
- Tool output is quality-checked before model reuse.

### Local command and Git lifecycle interface

The standalone `git` and `github` tools are removed from all built-in schemas,
registries and executors, including Server and Git-only request paths. Repository
and GitHub CLI operations use `git` and `gh` through an explicitly admitted shell.
Removing the tools does not grant shell authority: old allowlists, direct calls,
deferred selections and checkpoint carriers cannot restore the removed contracts
or translate them into shell commands. A Server-only request without a selected
shell provider no longer has these generic repository/GitHub operations. Shell
execution uses that provider's existing credential and isolation boundary; Server
credentials are never implicitly copied into a local shell.

The deferred `worktree(action=enter|exit)` tool owns session workspace switching
on CLI and User Runner. It invokes the existing worktree lifecycle and rollback
owner under the workspace writer lease. It has no legacy `git(action=worktree)`
alias. Ordinary worktree Git commands remain shell operations. Generic Server
executors cannot silently create a local session owner. Typed commit/stash tool
journals and their automatic compensation are removed along with their producers;
shell side effects retain the existing shell observation and recovery contract.
Every worktree lifecycle action requires explicit user approval before local or
Edge execution. In particular, `exit_action=remove` is destructive, and
`discard_changes=true` records requested behavior but does not grant consent.
A denied Cloud-to-Edge removal is not dispatched to the Edge provider.

Schema selection remains deterministic for unchanged bindings and policy. The
removed contracts change the tool prefix once during migration; ordinary calls
do not rewrite that prefix. Discovery and execution still enforce the selected
provider's current schema and authorization.

Internal Git/worktree helpers use one `BoundGitCommand` and the shared bounded
synchronous invocation runner. The Unix builder acquires directory and `.git`
identities with nofollow/openat, validates original repository configuration
without masking `core.worktree`, and rechecks acquired identities immediately
before launch. It sets a real absolute `GIT_DIR` and changes directory through
the acquired workspace FD. This replaces the former `ExactGitCommand` contract:
it does **not** promise an immutable dual-root filesystem view throughout Git
execution. A replacement between the last check and Git's path open, or an ABA
replacement, cannot always be detected; the selected provider's existing
isolation governs that window. Non-Unix retains its existing canonical-path
binding without claiming Unix descriptor guarantees.

All synchronous Git commands, including validation and worktree inspection,
share a timeout, fair nonblocking output collection and a 16 MiB output budget
(validation uses 64 KiB). They retain invocation-owner cleanup and never parse
truncated output as a complete result. Errors preserve whether failure occurred
before launch or after possible effects. A detected runtime binding change
invalidates attribution on the **captured original observation state**. Weak
scope ownership quarantines receipt attribution without disabling a settled
call's successor. This shared process owner does not itself grant the full Bash
filesystem/network sandbox to legacy typed helpers: existing provider admission,
managed-filesystem denial, mutation leases and isolation remain their owners.

The optional asynchronous `check-ignore` adapter retains startup binding checks;
it does not inherit synchronous invocation ownership or post-execution binding
validation. Its input/output exchange runs concurrently under one five-second
deadline after spawn, and cancellation drops the kill-on-drop direct child.
Synchronous repository validation precedes that exchange.
