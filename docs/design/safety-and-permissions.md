# Safety and permissions

> Status: target design contract.
> Last updated: 2026-07-07.

Safety owns permission policy, sandbox boundaries, side-effect classification, trust posture, and fail-closed behavior.

## Principles

- Local executor capability is never implied by server execution.
- Side effects are classified before execution.
- Permission denial is explicit and recoverable when possible.
- Unknown executor-gated capability is denied.
- Raw debug data is opt-in and short-lived.
- Safety failures should isolate the affected action before stopping the whole run.

## Side-effect classes

| Class | Examples | Default handling |
| --- | --- | --- |
| Read-only | read file, status query, introspect | allow if provider and policy allow. |
| Local mutation | write file, shell command, git change | require local provider and permission policy. |
| External read | web fetch, MCP read, API query | provider and data policy. |
| External mutation | deploy, ticket update, DB write | explicit approval/policy. |
| Dangerous action | destructive shell, credential exfiltration, unsafe network | deny or require high-trust approval. |

## Sandbox boundary

Server sandbox and Edge local workspace are different providers. A server sandbox does not inherit user-local authority unless explicitly bound.

A read-only child has an inherited execution ceiling, not a shell-text promise. Bash, PowerShell, detachable shell, and environment-lifetime background execution are unavailable in that child. This is deliberate: a general shell cannot be made safely read-only merely by mounting the workspace read-only because sockets, special files, helper binaries, and races can still cross the boundary. Typed read tools remain available, and command-backed skill verification is reported as unavailable rather than executed. Provider tools, project hooks, and skills must still respect the read-only ceiling. A parent approval cannot turn the ceiling into write authority.

## Plan mode

Plan mode blocks mutating and externally dangerous actions by default while preserving read/introspect ability.

## Permission sync

Permission decisions should be durable enough to explain later behavior and scoped enough to avoid accidental broad grants.

Record:

- user;
- session/run;
- tool/capability;
- provider;
- decision;
- scope;
- expiration;
- reason.

## Trust and audit

Safety-relevant decisions must emit C2/C3 facts:

- permission requested;
- permission granted/denied;
- policy blocked;
- sandbox boundary rejected;
- provider fallback selected;
- dangerous action refused.
