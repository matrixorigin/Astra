# Explain Analyze

> Status: target product contract.
> Last updated: 2026-09-13.

Explain Analyze makes one run/turn understandable while it is running and
after it has finished. It owns the graph projection, metric definitions, and
user experience. Runtime facts remain owned by the observation plane, and
durable storage, cursors, and reconnect handoff remain owned by run lifecycle
and session observability. Explain Analyze is not a second execution state
machine, event log, or report assembled from approximate UI timings.

## Product goals

1. **Stay attached to execution.** A user can observe a run as it advances,
   reconnect from a durable cursor, and continue with the same graph. Live
   delivery and replay must project the same ordered facts. If the retained
   history has a gap or the producer could not measure a boundary, the UI says
   so instead of presenting a complete-looking graph.
2. **Explain the work, not just elapsed time.** Show the run and turn structure,
   logical rounds, physical provider attempts and retries, tool batches and
   individual calls, waits/approvals, delegation, outcomes, and causal
   relationships. Preserve overlap so parallel work is visible.
3. **Make measurements interpretable.** Distinguish user-observed wall time
   from summed work, represent timestamps as offsets from the turn start,
   expose measured concurrency, and show provider token usage by attempt,
   including input/output and cache lanes when supplied. Mark unavailable or
   estimated values explicitly. Never sum overlapping spans and call that wall
   time, infer a critical path from missing edges, or count a retry/child run
   twice in totals.
4. **Share one useful representation.** A versioned Explain Analyze event
   stream supports live and replay consumers. CLI, Web, SDK, and exported HTML
   render that same graph contract; they do not reconstruct separate lifecycle
   semantics from text, logs, or arrival order.
5. **Make detail usable and safe.** Provide a clear overview first, then let a
   user inspect the timeline, parallel batches, token accounting, retries,
   causes, and bounded diagnostics. Raw prompts, reasoning, credentials, tool
   arguments, and large outputs are excluded by default. HTML exports are
   self-contained, escaped, and usable without a running server.

## Output quality contract

The overview must tell the user, in plain language, where the wall time went,
what materially delayed or blocked progress, which work overlapped, whether
retries changed the outcome, and which provider attempts consumed tokens. It
must distinguish measured execution from estimates and uncovered time. Each
finding should point to the graph evidence that supports it.

Only show a node in the default graph when it marks meaningful work, a material
decision, a wait, an outcome, or an explanation of cost. Do not expose internal
enum names, transport bookkeeping, repeated heartbeat/progress noise, or
generic labels such as “phase completed” as user-facing insight. Use stable
human-readable stage names and state what happened in that stage. Group
repetitive low-level detail under expandable attempts or calls; do not remove
the detail needed to explain duration, concurrency, retries, or usage.

An insight is valid only when its evidence is present. If the graph cannot
identify the bottleneck, critical path, token attribution, or parallel overlap
with adequate coverage, it must say the result is unavailable and why. Never
fill sparse data with confident-sounding boilerplate or unexplained zeroes.

The graph is a deterministic visual rendering of runtime facts; rendering it
does not call an LLM or generate narrative claims. Any LLM explanation is a
separate, explicitly requested product action and view. It must point back to
the graph facts it used, label uncertainty, and stay visually distinct from
the measured execution record.

The default view in both Web and TUI is an execution tree. Preserve the
information density of the README demonstration: context budget and source
costs, tool selection, memory selection, model attempts, tool calls, and usage.
Use clear hierarchy, aligned duration/token/status columns, and expandable
details. Unknown measurements remain visibly unknown. Group parallel work and
retries under their actual owners, with explicit dependency details. Do not
replace meaningful context information with a sparse list of timed stages.

Web adds subtle active-state and new-node animation, keyboard navigation, and
an inspector that remains visible when selecting nodes in long trees. A
switchable timeline is a complementary view of the same facts: one axis per
clock domain, with overlapping bars for concurrency. Use a stable status
palette with text labels as well as color: green for completed, red for failed,
amber for waits or blocked work, and blue for active work. Respect reduced
motion and keep both views calm while execution advances.

The quality fixture includes at least: a long provider wait followed by a fast
answer, a retried request with per-attempt token usage, parallel tools whose
summed work exceeds their wall envelope, an approval wait, and a trace with
missing timing coverage. Reviewers must be able to identify the slowest
material stage, actual parallelism, and usage attribution from the overview and
one level of detail, without interpreting internal protocol terminology.

## Terminal presentation

The TUI uses the same measured graph as Web and retains its existing tree
presentation as the primary view. Improve labels, indentation, aligned metrics,
and selective color without discarding the useful context sections. At narrow
widths, wrap stage details cleanly instead of squeezing labels or forcing a
wide timing chart. Details expose explicit dependencies, attempt usage, and
missing measurements. Live updates redraw in place at a bounded rate rather
than append a stream of reports. A timeline may be offered as an optional view.
Color supplements status symbols and text; respect `NO_COLOR`.

An independent `astra explain analyze <run-id>` entrypoint is the target for
opening an existing execution without reopening its chat. `--follow` attaches
to live facts with durable cursor recovery. Text output is stable and usable in
pipes; `--format jsonl` exposes the same versioned facts without terminal control
sequences; `--format html --output <path>` exports a self-contained interactive
report. These commands describe the intended interface, not current support.
The CLI must report unsupported or missing observations explicitly. This
entrypoint inspects actual execution; it does not execute the user's task again
or request an LLM explanation.

Interactive playback must distinguish recorded event replay from simply moving
an inspection cursor over measured intervals. Full recorded facts remain full
recorded facts; moving a cursor must not pretend to reconstruct information
availability at an earlier time. Terminal and Web views keep clock domains
separate and never fabricate future nodes from a plan.

## Graph contract

Explain Analyze is the product-facing account of what actually executed, in
the spirit of database `EXPLAIN ANALYZE`. It summarizes user-meaningful
runtime stages, actual attempts, overlap, outcomes, and token usage. It is not
a planned-work forecast and is not a raw trace viewer. Trace remains the
lower-level diagnostic evidence for cross-system spans, detailed causal
links, and support investigations. Explain Analyze consumes only the bounded
lifecycle facts it needs. When a deeper trace exists and the caller is
authorized, a stable correlation ID may link to it; turning Explain Analyze on
must not require trace capture or expose raw trace payloads.

The graph is rooted in a run and its user turns. Every node has a stable
identity, a kind, a safe label, an outcome, a producer, a clock domain, and
zero or more explicit parent or dependency edges. Arrival order and display
labels never define identity or causality. Parent containment and causal
dependency are distinct edges.

Model rounds and physical provider attempts carry numeric indexes as typed
fields. Consumers must not parse attempt numbers out of labels or node IDs.

Within one clock domain, execution nodes record monotonic start/end offsets from
that producer's turn origin and a measured duration. A terminal fact may carry
provider usage. Start, terminal, and usage facts use stable event and node IDs
so replay is idempotent. Child runs and restarted processes have separate clock
domains; their offsets are not compared to claim overlap or a shared critical
path. They can be aligned only through an explicit parent-observed interval
with a declared uncertainty. Otherwise each domain keeps its own timeline and
cross-domain timing is unknown. A missing start is represented as unknown; a
missing terminal on an active run remains active; a terminal run with
unresolved nodes is degraded. The run stream cursor orders durable events
across reconnects.

The first public Explain Analyze protocol is a versioned `explain_analyze`
event. Its facts cover at least:

| Area | Required facts |
| --- | --- |
| Turn boundary | admission, user-visible wall interval, final settlement |
| Preparation | context/history/memory assembly, prompt/cache preparation, compaction |
| Provider | every physical attempt, auxiliary call, retry/backoff, first token, outcome, provider usage |
| Tools | routing/admission, parallel batch envelope, each call's start/end/outcome |
| Waiting | approval, user input, provider interaction, and resume intervals |
| Delegation | parent-owned dispatch/fan-in plus child-run identity and its local clock domain |
| Terminal | success, failure, cancellation, interruption, or still-waiting state |

Coverage is explicit when a path is not instrumented. Token usage belongs to
the physical provider attempt that incurred it; estimates and reported usage
are separate. Preserve each provider's input/cache/output dimensions and their
declared overlap semantics. Do not derive fresh input by subtracting cache
lanes unless that provider's contract says they are subsets. Auxiliary judge
usage, retries, and continuation attempts remain separately attributable and
are counted once. A graph may report critical path and concurrency only when
its interval, clock-domain, and dependency coverage supports those
calculations; otherwise it reports unknown or local-domain-only metrics.

Schema version 1 is the new canonical Explain Analyze event contract. All
first-party producers and consumers change together; do not keep a generic
legacy `explain` payload, translate old phase events as a fallback, or maintain
parallel graph formats. The version field is part of this schema's evolution,
not a request to preserve superseded event shapes. Trace event schemas remain
owned and versioned by the observation plane.

## Presentation contract

- **CLI/TUI:** show live stage, elapsed wall time, completed stages, active
  parallel work, wait reason, and visible stream/replay degradation; allow
  opening the full graph after the turn.
- **Web:** render the graph and time axis interactively, with expandable node
  details, filters, and live updates from the run stream.
- **SDK:** expose the typed event and a reducer-friendly stream; callers can
  render without reverse-engineering event prose.
- **HTML:** export a standalone graph report from the same snapshot, with no
  remote scripts, fonts, or data requests.

## Correctness and failure behavior

- One runtime fact has one canonical producer; all clients consume its public
  projection.
- Durable append is ordered before event publication. Reconnect replays after
  the last durable index and hands off to live delivery without a missing
  interval or duplicate graph mutation.
- Structural start/terminal facts are emitted before/after their corresponding
  slow work. In the local loopback fixture, measure from runtime creation of a
  structural event through its application by the client graph reducer: p95 is
  at most 500 ms with one attached consumer and 100 structural events/second.
  A healthy consumer preserves its selected node and expanded branches across
  updates.
- Backpressure must either be repaired through the durable cursor or reported
  as a gap. It must not silently erase an explain node.
- If ordered durable append fails, use the existing run-lifecycle
  persistence-failure policy, cancel/terminalize as that contract requires,
  and show a degraded/failed observation state. Never continue to label a
  known-incomplete trace as complete or create an Explain-only recovery log.
- Cancellation, failure, approval wait, child-run completion, and restart are
  terminal or waiting outcomes in the same graph, not special text reports.
- Token totals reconcile to provider-attempt facts and the existing run-level
  usage authority. Unknown attribution stays unknown.
- Explain data uses the public redaction boundary. Verbose visualization does
  not grant access to raw prompts, chain-of-thought, credentials, or unbounded
  tool payloads.
- The graph identifies coverage gaps for admission, preparation, provider,
  tool, wait, child-run, and settlement paths. End-to-end wall time is shown
  beside measured accounted time so instrumentation gaps remain visible.

## Acceptance scenarios

- A live run can be disconnected and resumed from its last event index; the
  reconstructed graph equals uninterrupted delivery.
- Duplicate delivery is idempotent. A missing or conflicting fact is surfaced
  as degraded rather than guessed from neighboring timestamps.
- Parallel tools visibly overlap, their batch wall envelope is distinct from
  summed tool work, and retries show separate provider attempts.
- Input/output/cache token usage reconciles per provider attempt without
  double-counting continuation or delegated child runs.
- A completed HTML export contains the graph and assets offline and safely
  displays hostile tool labels or diagnostics as text.
- A deterministic 10,000-node fixture uses at most 100 MiB for graph data and
  client state, paints the first 500 visible nodes within 1 second, and applies
  incremental updates within 50 ms at p95 on the documented Web test runner.
  Initial detail is windowed and long histories can be paged without resetting
  keyboard focus, selection, or expansion state.
- Fault injection covers producer crash between start and terminal, process
  restart with a new clock domain, clock offset/drift between child and parent,
  durable append failure, slow/full consumers, reconnect at each structural
  boundary, and the replay-to-live handoff. No case may yield a complete graph
  with invented timing or missing execution nodes.
