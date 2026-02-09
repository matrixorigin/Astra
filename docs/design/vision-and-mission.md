# Vision and Mission

## Vision

**A platform of digital employees** that observe, operate, test, and develop across code and test repositories—scaling routine engineering work without scaling headcount—while **every interaction becomes data**: stored, analyzed, and used to continuously improve agents, prompts, memory, and workflows.

## Mission

Build an agent system that:

1. **Serves** like many engineers in parallel: watch repos and Actions, summarize PRs, run and interpret CI/regressions/benchmarks, create and deploy test configs, triage and fix bugs, and participate in review—all driven by **skills**, **rules**, and an LLM that understands natural language.
2. **Trains** from its own service: conversation history, cross-session summaries, ratings, and task outcomes are **recorded and analyzed**; this data fuels **fine-tuning** (e.g. per-user or per-role models), **prompt/context/memory optimization**, and **retrospective comparison** of strategies—so serving and optimization are one loop.
3. **Runs on a data engine**: all persistent state lives in **MatrixOne**, with deep use of **clone**, **Git for data**, **multimodal**, **HTAP**, **RBAC**, and **row-level access control**—so we both **accumulate** data and **consume** it safely and efficiently for analytics, tuning, and access control.

---

## Technical foundations

### Sandbox

- **Safe execution** for code runs, tests, and tool calls: agents operate inside a **sandbox** so that arbitrary commands and file access are isolated and auditable.
- Sandbox boundaries (e.g. workspace access, network, elevated permissions) are explicit and configurable, aligning with modern agent frameworks that treat execution safety as first-class.

### Conversation history and multi-session context

- **Conversation history** is first-class: every session is stored and available for retrieval and summarization.
- **Cross-session summarization per user**: the system can aggregate and summarize one person’s activity across multiple sessions (e.g. “this user’s recurring questions,” “tasks completed over the last N sessions”), so context is not lost between sessions and can be used for personalization and better prompts.
- History is **easy to record, query, and process**—part of the data-engineering story below.

### Data engineering: record, analyze, use

- **Record**: Every interaction, every task, every evaluation and score is storable (e.g. in MatrixOne): requests, responses, tool calls, outcomes, and human feedback.
- **Analyze**: Data is queryable and analyzable—which prompts work, which memory operations help, which context parameters correlate with success—so we can compare runs, A/B test strategies, and retrospect.
- **Use**: The same data supports:
  - **Per-user (or per-role) fine-tuning**: e.g. a model tuned on one person’s multi-session conversations.
  - **Prompt / context / memory optimization**: tuning prompts, context window usage, memory operations and parameters from historical performance and scores.
  - **Small-model tuning**: training or adapting smaller models for specific tasks using the accumulated feedback.
- **Periodic or automatic optimization**: workflows, memory policies, context assembly, and model choices can be updated on a schedule or triggered by analysis—so the system improves without manual intervention every time.

### Storage and integration: MatrixOne

- **All durable storage** is backed by **MatrixOne**, so one engine holds conversations, sessions, evaluations, configs, and derived datasets.
- **Deep integration** with MatrixOne capabilities:
  - **Clone**: efficient copy and branching of datasets for experiments and rollback.
  - **Git for data**: version and diff data like code (e.g. prompt versions, memory snapshots).
  - **Multimodal**: store and query text, embeddings, and other modalities in one place.
  - **HTAP**: operational workloads (serving, writes) and analytical queries (aggregations, tuning analyses) on the same data.
  - **RBAC and row-level access control**: secure, multi-tenant data so that per-user or per-team data is only visible to the right principals—critical when conversation and feedback data are sensitive.

**Result**: Data is **precipitated** from service, then **consumed** for analytics, optimization, and access-controlled product features—a single data platform for both “run the agents” and “improve the agents.”

---

## What digital employees do (capabilities)

### Code repos and PRs

- Watch **code-repo Actions** (e.g. auto-close/merge rules, PR lifecycle, CI).
- **On request**: summarize a PR (e.g. status, flaky CI, conflicts); with **human approval**, create an issue using **specified skills, templates, and rules** so each issue is consistent and traceable.

### CI and regression repos

- Watch **CI and regression repo Actions** (e.g. daily runs, scheduled jobs); surface failures **on demand** or in near real time.
- **Link to observability**: point to **Loki** (logs) and **metrics**; generate a **temporary entry** (link or small dashboard) so employees can jump to the failing run.
- **Issue when confirmed**: if a failure is confirmed as a real bug, create an issue (again with skills, templates, rules).
- Agents behave as **virtual employees**: driven by **skills** and **rules**, using an **LLM** to interpret natural-language commands (“check today’s daily,” “summarize PR #123”) and decide what to do and how.

### Broader scope (observe, operate, test, develop)

- **Observe**: repos, CI status, daily regressions, benchmark results, deployment and run status.
- **Operate**: author and deploy test configs, trigger runs, deploy and monitor.
- **Test**: run tests and benchmarks, interpret results.
- **Develop**: discover and triage bugs, implement fixes, participate in review.

---

## Feedback, memory, and continuous improvement

- **Service is training**: the process of serving is part of training; every interaction and task can be **evaluated and scored** (explicit or implicit), and feedback is stored.
- **Context and memory engineering** are first-class: we invest in how context is assembled (prompt, retrieved memory, session summary) and how memory is updated and queried—in the spirit of **moltbot-style** memory (e.g. hybrid search over memory files and sessions) and **structured prompts** (clear sections for tooling, skills, workspace, sandbox).
- **Optimization engine**: the feedback loop drives **regression quality**, **release decisions**, and **product iteration**—continuously improving both agent behavior and the engineering process.
- **Current relevance**: this aligns with the broader trend of **agent memory**, **evaluation and scoring**, **data-centric AI**, and **RAG/retrieval**—treating “data from usage” as the main lever for improving agent systems.

---

## In short

**mo-dev-agent** is a platform for **many digital employees** that:

- **Serve**: watch code and test repos, CI, and regressions; summarize PRs and create issues with skills/templates; surface failures and link to Loki/metrics; run tests and benchmarks; triage and fix bugs and join review—all via **sandboxed** execution, **skills**, **rules**, and an **LLM**.
- **Use conversation and history well**: conversation history and **multi-session summaries per user** are stored and used for context and personalization.
- **Treat data as the engine**: **record** every interaction and task, **analyze** to tune prompts, context, memory, and small models, and **retrospect** to compare strategies; **all storage in MatrixOne** with **clone**, **Git for data**, **multimodal**, **HTAP**, **RBAC**, and **row-level access control**.
- **Improve continuously**: **periodic or automatic** optimization of workflows, memory, context, and models, so that serving and training are one loop and the system becomes the **engine** for regression, release, and iteration.
