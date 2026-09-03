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
