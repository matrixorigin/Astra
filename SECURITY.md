# Security Policy

Astra is an agent runtime that holds credentials, brokers tool execution inside
private environments, and records audit evidence. Vulnerabilities in it can
affect systems well beyond Astra itself, so please report them privately.

## Reporting a vulnerability

**Do not open a public GitHub issue, discussion, or pull request for a security
problem.**

Email the Astra maintainers at `xupeng@matrixorigin.io` with
`[Astra security]` in the subject line. If you do not receive an
acknowledgement within five business days, send a follow-up rather than
disclosing publicly.

Please include as much of the following as you have:

- A description of the issue and why you believe it is a security problem.
- The affected component: `astra-server`, `astra` CLI, `astra-edge`, the
  TypeScript SDK, the Web dashboard, deployment manifests, or a specific crate.
- Astra version or Git commit, deployment profile (CLI + Server, Server-only,
  or Server + Edge / User Runner), and operating system.
- Reproduction steps or a proof of concept, and the impact you were able to
  demonstrate.
- Any suggested mitigation.

Redact API keys, tokens, passwords, customer data, and private URLs before
sending, exactly as described in [SUPPORT.md](SUPPORT.md).

## Scope

Astra is pre-1.0 and public interfaces may change. Security fixes are made on
`main`; there are no maintained release branches yet.

Reports that are in scope include, for example:

- Authentication or authorization bypass in the Server or admin surfaces.
- Escaping the policy and admission path so a tool executes without being
  admitted, or executes outside its bound identity, workspace, or permission
  scope.
- Runner registration or dispatch flaws that let work be routed to, or accepted
  by, an unintended Runner.
- Disclosure of credentials, tokens, context, or trace and audit records across
  identity or tenant boundaries.
- Tampering with audit or trace records that are meant to be accountable.
- Injection, path traversal, or sandbox escape in tool execution.

The following are generally **out of scope**:

- Model output quality, hallucination, jailbreaks, or prompt injection that
  does not cross a policy or permission boundary that Astra is meant to
  enforce. If prompt injection causes a tool to execute outside its admitted
  scope, that *is* in scope — please report it.
- Example and template files (`.env.example`, `.models.yaml.example`,
  `config/server.toml.example`, deployment examples) used verbatim in
  production. These are starting points; see the
  [production guide](docs/quickstart/production.md).
- Findings that require an attacker who already has administrative access to
  the host, the database, or the Astra admin account.
- Known vulnerable dependencies without a demonstrated path to impact in
  Astra. Dependency updates are handled by Dependabot and `make audit`.
- Reports produced by an automated scanner with no analysis of exploitability.

## Handling

We aim to acknowledge a report within five business days, confirm or reject it
with a severity assessment, and agree a disclosure timeline with the reporter.
Astra is a community-supported open-source project: there is no service-level
agreement, and timelines depend on severity and complexity.

We support coordinated disclosure and will credit reporters who want to be
named. Please give us a reasonable opportunity to ship a fix before publishing
details.

## Hardening

Operators are responsible for the deployment boundary. Before exposing Astra
beyond a development machine, review the
[production guide](docs/quickstart/production.md) and the
[configuration reference](docs/reference/configuration.md), and in particular:

- Generate real values for `ASTRA_JWT_SECRET`, `ASTRA_TOKEN_ENCRYPTION_KEY`,
  `ASTRA_RUNTIME_ROOT_SECRET`, and `MEMORIA_MASTER_KEY`. Never ship the
  template values.
- Keep `.env`, `.env.production`, and `.models.yaml` out of version control.
- Bind development services to loopback. `ASTRA_BIND_ADDRESS` defaults to
  `127.0.0.1` for a reason.
- Remember that Runner placement controls where execution happens, not where
  data goes. Model endpoints, context disclosure, and tool-result handling
  remain explicit policy choices.
