# Permission sync

> Status: target design contract.
> Last updated: 2026-07-07.

Permission sync defines how permission decisions remain consistent across Web, CLI, Edge, Server, MCP, and resumed sessions.

## Ownership

This document owns:

- permission grant shape;
- cross-surface permission propagation;
- local prompt vs cloud audit boundary;
- revocation and expiration;
- permission diagnostics.

Safety policy belongs to [safety-and-permissions.md](safety-and-permissions.md). Provider routing belongs to [capability-system.md](capability-system.md).

## Principle

```text
Permission is scoped authority, not a boolean remembered by the UI.
```

## Grant shape

A permission grant should record:

```text
grant_id
user_id
session_id
run_id
capability
tool_name
provider_id
resource_scope
action_scope
decision
expires_at
created_by_surface
audit_event_id
```

## Scope

Permission scope must be explicit:

- one tool call;
- current run;
- current session;
- workspace path prefix;
- command pattern;
- MCP server/tool;
- time-limited approval;
- deny rule.

## Cross-surface behavior

- Edge may collect local approval for local execution.
- Cloud must receive enough fact data to explain the decision later.
- Web should show pending approval state when Edge waits for local user input.
- Resume should not broaden old approvals.
- Revocation must prevent future dispatch even if a stale client UI says allowed.

## Plan mode

Plan mode denial is policy, not permission absence. Diagnostics should distinguish “permission missing” from “plan mode blocks mutation”.

## Revocation

Revocation should be durable and provider-visible. Revoked grants should not be silently re-enabled by reconnect or resume.

## Test obligations

- Approval scoped to one run does not leak to another run.
- Revocation wins over stale local state.
- Web can observe Edge waiting for permission.
- Plan mode denial is reported distinctly from permission denial.
