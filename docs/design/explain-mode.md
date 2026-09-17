# Explain Analyze

> Status: target product contract.
> Last updated: 2026-09-14.

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

Web and TUI preserve the execution tree and the
information density of the README demonstration: context budget and source
costs, tool selection, memory selection, model attempts, tool calls, and usage.
Use clear hierarchy, aligned duration/token/status columns, and expandable
details. Unknown measurements remain visibly unknown. Group parallel work and
retries under their actual owners, with explicit dependency details. Do not
replace meaningful context information with a sparse list of timed stages.

Web defaults to a full-width, monospace execution tree in the README style.
Stage names, measured duration, and relevant outcomes share one line; context
source estimates retain their token units rather than appearing as durations.
A compact heading and inline time/token summary precede the tree. Measured
provider usage, context estimates, and missing coverage remain distinct.
Details expand immediately below the selected row. Search keeps matching
stages with their ancestors; clearing it restores collapsed branches. Copy
exports the full recorded text hierarchy, independent of the current collapse
or search state. Arrow keys navigate visible rows and expand or collapse a
branch; Enter opens its recorded facts. Live updates preserve expansion,
selection, focus, and scroll rather than reopening branches on each event.

Timeline is a secondary view for investigating overlap, with a separate axis
per clock domain. It does not add a time bar to every row in the default tree.
An optional node graph uses only observed parent and dependency edges: parent
edges express containment, while dependency edges express prerequisites.
Neither sibling order nor timestamps create a causal dependency or recovery
relationship. Failure facts remain visible after later work succeeds. Only
recorded explanations and explicit evidence relationships may be displayed;
a producer that supplies no failure reason must not acquire an invented one.

Use readable primary text, muted connectors, restrained metric emphasis,
amber for waiting or unknown information, and red for failures. Status text
remains available alongside color. New rows enter in place; live elapsed
values carry an estimate marker and settle to measurements when terminal
facts arrive. Respect reduced motion and keep historical records static.

Live animation requires an explicit active observation from the host surface.
Missing terminal facts alone do not establish that execution is still running.
Saved HTML and historical messages remain static and label unrecorded ends.
Independent turn outcomes aggregate without imposing order on clock identifiers;
different outcomes remain mixed. Token subtotals state how many observed
requests supplied usage, retain known lanes, and leave unreported lanes unknown.
Context assembly and request budget estimates remain separate from provider
usage and identify their estimation scope.

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

The `on` mode keeps this tree concise while retaining measured timing, overlap,
wait, and provider-usage facts. `verbose` keeps the same rows and adds the
request-budget basis, context-source estimates, dependencies, and coverage
diagnostics. A mode change changes presentation detail, never the measured
facts or their parent/dependency relationships.

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

For tools, the admission interval ends at the actual dispatch boundary. An
Edge approval wait is a nested `wait` interval; the `tool_call` interval starts
when the server begins local transport or delivers a committed Edge request,
and ends when the result is observed. Admission and wait intervals remain visible but
do not count as parallel work. Tool-call duration is end-to-end wall time and
may include external I/O. Explain Analyze never labels that whole duration as
I/O wait: a separate I/O breakdown appears only when the execution source
reports a distinct measured interval. Detailed cross-system I/O spans remain
the responsibility of Trace.

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

A terminal `turn` fact carries the producer's known `coverage_gaps`. The server
measures Edge approval waits. Server-internal approval waits are not yet
separately instrumented and remain part of dispatch-to-result wall time.
The approval coverage gap therefore remains. Other known gaps include user-input
waits, provider retry backoff, time to first token, child-run intervals, and the tool I/O wait
breakdown when an execution source does not provide it.
Coverage gaps do not make observed facts structurally inconsistent, but they
do prevent a renderer from presenting observed overlap as total concurrency.
Render the measured overlap as a lower bound and name the unmeasured boundaries.

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

## Report artifacts and agent analysis

An Artifact is a typed document reference, not a synonym for a string or a
filesystem path. Its contract separates four things:

| Field | Meaning | Current Explain Analyze value |
| --- | --- | --- |
| `artifact_schema_version` | Version of the artifact envelope | `1` |
| `artifact_type` | What the document means | `explain_analyze_snapshot` |
| `content_type` | How the document is encoded | `application/json` |
| `storage` | Where the runtime keeps the bytes | `database_session` for server runs; `local_session` for a local host adapter |
| `representation` | Which canonical/derived form is addressed | `canonical` |
| `status` | Whether the snapshot is usable | `in_progress`, `complete`, `partial`, or `unavailable` |
| `size_bytes`, `checksum_sha256` | Bounded integrity metadata for a readable snapshot | present for `complete`/`partial` |

The long-term contract has four independently evolvable layers:

| Layer | Question it answers | Examples |
| --- | --- | --- |
| Envelope | What is this and who may use it? | type, status, session scope, redaction, retention, capabilities |
| Payload | Which facts or bytes are represented? | Explain events, a derived tree, HTML, a trace bundle, a user file |
| Representation | How should a consumer read or render it? | canonical JSON, text, Markdown, HTML, binary |
| Locator | Where can an authorized reader obtain it? | opaque session handle, database ID, local adapter, signed object fetch |

`artifact_type` identifies the semantic document contract; `content_type` and
`representation` identify its encoding and presentation form. A derived
representation carries provenance back to the canonical artifact and never
becomes a second source of execution facts. A capture can therefore expose a
canonical JSON artifact, a Markdown/HTML view, and a trace or debug bundle as
separate typed references without making the renderer or agent understand
storage-specific URLs.

The envelope is designed for more than the current four states. A backend may
later add `expired`, `deleted`, or `quarantined`, but a consumer must treat an
unknown status as unavailable and must not fall back to an older pointer. The
same rule applies to unknown artifact types, representations, storage backends,
and capabilities: fail closed, preserve the metadata for an authorized
diagnostic surface, and never reinterpret it as a text document. A future
multi-artifact host will index artifacts for one turn by type and
representation; the `latest` pointer is only a discovery aid and is never the
artifact identity. The execution host publishes one canonical snapshot; a
client Markdown or HTML file is only a derived presentation copy.

The current implementation persists one canonical JSON snapshot containing the
bounded, redacted versioned event set, run/turn identity, event schema version,
artifact envelope version, and delivery status. The latest pointer records its
byte size and SHA-256 digest. Plain text, Markdown, and HTML are representations derived
from that snapshot; they are not separate sources of Explain facts. The tree,
timeline, text export, and HTML report must therefore agree on node identity,
timing basis, and coverage. Saving a Markdown or HTML rendering alone does not
make a report available to a later agent turn.

Server execution persists its canonical snapshot in the owner/session-scoped
database artifact store and wires the same bounded reader into `introspect`.
The CLI/TUI may additionally persist a local companion for a human operator;
that companion is not injected into server model context. A local-only host can
use the local store and reader when it owns both execution and model calls.

The execution topology determines which host owns the artifact bytes and which
surface may render them:

| Topology | Live and replay facts | Human rendering | Next-turn artifact analysis |
| --- | --- | --- | --- |
| CLI/local runtime | Local typed stream and session journal | CLI/TUI may write a local HTML companion by default (or explicit Markdown/text) and print its path | The local `introspect` reader can consume the opaque session handle |
| Server + TUI/CLI | Server lifecycle is authoritative; the client consumes the same typed stream and durable replay | The client may render a local companion for the human, but must not treat its path as server authority | The model can recover only through a reader backed by the same server/session store; otherwise the required context reports the artifact as unavailable |
| Server only | Server emits the versioned stream and durable run cursor | Web/SDK owns rendering or export; no server process writes a user's local path | The server-owned database handle is discovered on the next turn and read through server `introspect` |
| Server + Edge | Server and Edge facts retain producer, clock, parent, and gap metadata; reconnect uses the durable server cursor | The attached client renders one graph from merged facts; Edge never creates a second Explain semantics | Edge-local paths stay local; recovery uses the authorized host artifact backend and reports missing cross-host readers explicitly |

Client-side rendering must therefore degrade to “shown locally, unavailable to
the remote model” when the host stores differ. It must never put a physical
client path into server model context or claim that a local handle is readable
from a remote Server. The typed event stream remains useful in every topology,
even when the richer artifact reader is not yet installed.

The current implementation intentionally exposes one capability, `read_window`,
through both the server and local bounded readers. Future capabilities such as
`download`, `render`, or `cite` must be granted by the host and represented in
the envelope; a content type alone never grants them. Binary artifacts require
a byte/range reader or an authorized download capability rather than being
decoded as UTF-8. Streaming artifacts use a cursor and expiry contract instead
of pretending that a partial stream is a completed snapshot.

The model-facing value is an opaque, session-scoped handle. The server and
local readers support UTF-8 JSON windows with `offset` and a bounded
`max_bytes` (64 KiB maximum), and return a continuation offset. They never
expose a physical path. A failed write publishes an `unavailable` status for
that run and turn when the store is reachable. Server discovery is bound first
to the latest durable `run_started` record that explicitly requested Explain
Analyze, then to that run's deterministic artifact identity; a missing or
invalid artifact is reported as unavailable and never falls back to an older
run. The local index uses the same fail-closed status model for the host it
owns.

Future storage backends fit the same reference contract. A trusted local-path
backend may be used by a host adapter for files it owns; an S3-compatible
backend would use a tenant/session-scoped object key, checksum, retention
metadata, and an authorized server-side reader or short-lived signed fetch.
Neither a raw local path nor an arbitrary `s3://` URL is accepted as an agent
authority. Storage location is an implementation detail behind the handle,
while type, media type, status, size, checksum, and permitted read operations
remain explicit metadata.

Future storage adapters must also define quotas, retention, cleanup, concurrent
publication, and audit behavior. A local file can disappear, an object store
can return a stale version, and a signed URL can expire between pages; each
case is an explicit unavailable/expired result with no silent fallback. Those
adapters must check tenant, user, session, and run ownership before resolving a
locator. The server boundary is the authenticated user and session owner; the
local boundary is the active session, local host, and `source_policy` check
described above.
Derived artifacts retain their parent identity and checksum, while trace and
debug artifacts keep their own authorization and redaction policy. This keeps
large files, provider captures, screenshots, exports, and future multimodal
payloads on the same reference model without widening Explain Analyze into a
raw trace or file browser.

The server advertises the latest handle through the typed required-context lane
on the next turn. The CLI/TUI may also print the path of a derived HTML report
by default, or an explicitly selected Markdown/text representation, for the
human operator. That path is a presentation affordance; the model receives the
server-owned opaque handle and reads the canonical artifact through the bounded
reader. The physical path is never treated as model authority. A client-local
companion is explicitly unavailable to a remote model when no shared artifact
backend exists.
The next turn receives only a short artifact handle through `introspect`. An
agent that is explicitly asked to analyze the previous explain report reads
that handle through the bounded artifact window API, then cites the recorded
node and event identities in its answer. The entire event set is never added
to the prompt automatically. Missing, partial, or unavailable
artifacts are reported as such, and an Explain artifact never grants access to
raw prompts, chain-of-thought, credentials, tool arguments, tool output, or
trace payloads.

## Correctness and failure behavior

- Report publication has its own typed `artifact_publication` result, separate
  from task completion and graph coverage. A successful result carries the
  server-session handle; a failure carries a bounded safe reason. The current
  stream receives this result before its terminal frames. If retaining the
  result also fails, `recorded=false` makes that limitation explicit. A local
  derived-report path never substitutes for server publication success.
- Failed publication is a run observation, not an immutable empty snapshot.
  Missing reports can be recovered from the exact completed run's durable
  facts on discovery or buffered-completion resume. Paused or cancelled runs
  cannot promote buffered successful facts into a completed report. Recovery
  preserves turn and generation identity and records its publication result;
  it does not rerun the model or overwrite a successful conflicting snapshot.
- Artifact discovery is background capability metadata. The agent explains
  unavailability when asked about that report, rather than inserting unrelated
  storage warnings into ordinary answers. Clients surface failures when they
  occur, independently of what the model chooses to say.
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
