# Skill capability evolution

> Status: target design contract.
> Last updated: 2026-07-07.

Skill capability evolution defines how Astra moves from prompt-only skills to verifiable, provider-aware capabilities.

## Principle

A skill is not just text. A mature skill is a versioned capability package with instructions, tools, resources, policy, tests, and observable outcomes.

## Skill maturity levels

| Level | Meaning |
| --- | --- |
| L0 Prompt | Instruction-only skill. |
| L1 Structured prompt | Skill with declared inputs, outputs, and examples. |
| L2 Tool-backed | Skill exposes or depends on tools through capability decisions. |
| L3 Resource-backed | Skill owns resources, references, templates, or MCP bindings. |
| L4 Evaluated | Skill has tests, rubrics, and regression cases. |
| L5 Governed | Skill has versioning, rollout, permissions, telemetry, and rollback. |

## Skill package contract

A skill package should declare:

```text
skill_id
version
capabilities_required
providers_allowed
instructions
resources
tools_or_mcp_bindings
input_contract
output_contract
permission_requirements
evaluation_cases
compatibility
rollback_policy
```

## Provider boundary

Skill tools do not bypass provider decisions. A skill may request a capability, but the current session/provider/policy decides whether that capability is visible and executable.

## Evolution workflow

```text
author -> validate -> evaluate -> publish -> activate -> observe -> tune -> version
```

Activation may be scoped by user, workspace, agent, environment, or policy.

## Compatibility

Skill changes must declare whether they are:

- instruction-only;
- schema-compatible;
- schema-breaking;
- provider-requirement-changing;
- permission-changing.

Provider or permission changes require stronger review and regression gates.

## Observability

Track per skill version:

- invocation count;
- success/failure rate;
- tool-call validity;
- user correction rate;
- provider fallback/block rate;
- token cost;
- regression failures.
