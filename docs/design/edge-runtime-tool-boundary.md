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

A skill fork isolates model context while inheriting the parent's selected
execution boundary. For a selected Server sandbox, its file executor and
sandbox policy use that exact root; a missing or relative root is rejected.
Edge and Cloud workspace paths remain with their respective providers and
must not become Server-local filesystem roots.

A managed Server sandbox records the selected executor identity in its
Workspace source. Its owner, session, executor, root and physical capability
are immutable; a later run may update only nonphysical metadata. Patch export,
apply and commit resolve the current Work binding through that same selected
provider. Another instance must skip the operation without claiming it,
changing its recovery time, or turning a missing local directory into failure.
After waiting for the physical workspace lease, the executor rechecks the
binding and provider identity before durable claim or Git access. Existing
operations with unknown external effects remain reconciliation work; affinity
does not authorize replay of a mutation.


## Edge boundary

Edge/CLI local runtime may expose local capabilities only within declared authority:

- selected workspace root;
- permission policy;
- sandbox mode;
- user identity and local environment;
- provider health and binding lifetime.

Path authority must be checked before execution. A model-provided path is not authority by itself.

`read_file` uses inclusive, one-based line bounds. When both bounds are
positive but reversed, both local and Server executors read the ordered range;
missing bounds and invalid values keep their usual meaning. Tool status and
deduplication use the same ordered range while the original arguments remain
available for audit.

### Foreground Bash output

The CLI keeps command output in the existing private task capture, bounded by
the task retention limits. When an inline result cannot represent all output,
it provides the task ID for `task_output` if those controls are available.
Timeout, cancellation and capture failures identify output as incomplete while
preserving the observed execution result. The task's output can be evicted
under the existing retention budget or become unavailable after session close.

The foreground Edge executor has no task reader. It returns a bounded, redacted
preview and states when output is truncated or incomplete. Neither executor
writes command output to the workspace unless the command explicitly does so.

File outlines use the shared AST parser for supported languages, including
Python stub, JavaScript module and TypeScript module extensions. Unsupported
languages and comment-only files do not acquire signatures from text-prefix
heuristics. Outline signatures are redacted for display and do not establish
full-file delivery or authorize replacement. A bounded prefix is identified as
partial content; its measured line count does not describe the unread suffix.

Credential read views preserve assignment syntax and replace only the secret
value with an opaque reference. Presentation boundaries retain complete,
canonical non-secret reference envelopes across processes; this does not grant
edit authority. Only the source executor can authenticate the reference and
resolve one unchanged, unambiguous source span. Adjacent raw secrets remain
subject to redaction, and display-only markers cannot authorize edits.


### Server-owned execution

The CLI admits one Server-owned loop; local tools execute only through the
Server's authenticated callback requests. Missing terminal execution evidence
does not transfer continuation authority to the CLI. The adapter preserves
observed results and usage, reports a typed protocol failure, and leaves an
unsettled run under Server authority. Explicit user cancellation and output
failure continue to use the existing physical-run cleanup path. The adapter
must not replay tools or open another admission to complete an incomplete stream.

After local execution, the CLI retries transient result-delivery failures with
the same scoped request and result hash. The tool itself is never replayed.
Delivery retries remain bounded and stop on cancellation or a permanent
rejection; an already-started credential rotation is settled by its existing
owner even if the caller stops waiting. Only a Server acknowledgement marks
the result delivered. After the receipt window has elapsed, a short best-effort attempt
may still retain a late result; its acknowledgement does not prove the current
round consumed it. Unrepeated delivery makes no additional queries. Delivery
uses at most 16 concurrent callbacks per CLI process and at most four active
callback tasks per stream, without holding a tool-execution permit. Queued
results retain their original delivery deadline; congestion can therefore
cause a result to expire without being sent. Completed outputs are shared with
their callback rather than copied while waiting, but their total size and the
number of concurrent streams are not bounded by these limits. The limits are
per process, not a cluster-wide bound on Server work.

While a tool batch runs, the SSE reader pauses further reads after accumulating
one MiB of later executable work. It processes the whole already-received
network chunk first, including a terminal event in that chunk; consequently
the pending size can exceed the threshold by the size of that chunk. Terminal
events in later chunks wait while backpressure applies. Callback failure
warnings are best effort when the output channel cannot accept them; the final
typed failure remains authoritative.

When durable dispatch is configured, an uncertain write does not release a
local waiter: the write may have committed before its acknowledgement was
lost. The same result can be retried; a different canonical result after
credential redaction is rejected. Explicit local-only hosts continue to use
the local ledger.

Completed results can be delivered independently, including results from the
same run. The Server consumes locally available results as they arrive and
assembles the model-facing batch in its original request order. Each pending
tool has a local waiter; completed outputs and progress events can coexist in
memory while a sibling is still running. Normal waiting performs no recurring
result reads. After the local receipt deadline or uncertain cancellation, an
existing exact durable read may recover a callback accepted by another Server
instance. If that read fails or the result arrives later, its outcome remains
unconfirmed for the live turn. Final reads remain
serialized per batch: if many results time out while the durable store is
slow, each read gets its own one-second timeout, so the batch can wait
proportionally longer. In local-only mode, the bounded replay cache can evict
an already consumed receipt before its HTTP retry arrives under load.

If delivery cannot be confirmed, the Server retains run authority; a client
disconnect does not establish that an outstanding tool stopped. When multiple
callback deliveries fail, the existing interruption reports their scoped run
and request identities and errors, with authentication failure taking priority.
An unconfirmed side effect must not be repeated to manufacture a receipt.

A Server-issued read-only ceiling belongs to its exact tool request and travels
through parsing, batching and execution. Stream completion cannot clear that
ceiling while the request remains pending. Local approval cannot widen it.

Bash command time is carried by the Server's `command_timeout_cap_ms` request
metadata. The separate delivery deadline includes callback settlement time and
must not extend the command budget. Missing or invalid command authority rejects
a Bash request. Model arguments remain unchanged for approval, invocation identity
and journal evidence; runtime controls are never injected into those arguments.

Ordinary shell execution uses the selected provider's permission, sandbox and
process-coordination boundaries. Optional bounded observation is evidence, not
an execution prerequisite. The executor records pre-start rejection separately
from started success, failure or cancellation; unavailable observation cannot
turn either into a permission denial. A started command deadline carries typed
`tool_timeout` evidence even after its process scope is settled. Cancellation,
timeout and lost outcomes can leave partial effects and must not be replayed
merely to obtain evidence.

Command output can establish a current state, including an already-satisfied
request or an absent directory, without claiming this invocation changed it.
Neither a zero exit status nor a generic filesystem delta proves arbitrary task
correctness. Domain tools and canonical Work retain their own result contracts.
Inspection, diagnostics and checks use the same command tool. The executor
reports the actual output and exit status without a whole-workspace fingerprint
or a separate verification mode. Declared checks are explicit runtime contracts;
a successful invocation does not prove workspace exclusivity or that no other
process will write later.

### Rename authority

The CLI may modify symbol references only through an active semantic LSP backend
with a position-based rename request. A text search cannot prove symbol identity
across scopes, comments, or strings and must not authorize file modifications.
An explicit `dry_run=true` request may produce a clearly labeled non-semantic
text preview; it never applies edits or claims semantic reference coverage.
Missing or failed semantic backends return an actionable error, not a text-based
mutation fallback.

### File-edit publication

File-edit tools commit the prepared bytes. Write and replacement requests use
the shared deterministic line-ending and trailing-newline normalization;
LSP edits publish their computed candidate exactly, preserving bytes outside
the selected ranges. They do not implicitly launch formatters. Formatting and build checks are explicit tool calls through the
existing owned shell lifecycle, with its approval, deadline and cancellation
boundaries. A successful `write_file` proves publication, not syntax validity;
replacement tools retain their independent parser-based structural checks and
explicit `allow_structural_change` control.
Text replacements reject invalid UTF-8 before changing file bytes or recording
journal entries, rather than silently replacing unrelated bytes during decoding.

The CLI file-state guard compares current bytes with the captured content
digest; a modification-time change alone does not invalidate read authority.
Only a confirmed missing target may proceed without tracked content identity.
Metadata, content-read and state-lock failures reject validation. Whole-file
overwrites still require complete content authority, and publication retains
its independent prepared-preimage check.

Single and batch replacement candidates share the same matching owner, including
quote-normalized ambiguity rejection, quote-style preservation and structural
validation. Batch edits resolve sequentially against the current candidate;
exact anchors may target an earlier edit's result, while a stale fuzzy anchor
cannot overwrite a previously fuzzy-edited region. Region coordinates follow
byte-length changes between edits. A failed preparation publishes no files.

CLI same-file batches supply an authorized bound target and captured UTF-8
preimage to the shared candidate owner. The physical target determines content
normalization; the logical spelling is a display label. CLI publishes those
candidate bytes exactly and records the committed bytes in its local journal.

The shared `fs_ops::Prepared*` owner stages files using exclusive sibling
creation and checks the captured preimage immediately before publication.
New files use no-clobber publication. The workspace lease coordinates Astra
writers; preimage checks are not an OS compare-and-swap against external editors.
Multi-file edits prepare and validate all candidates before publishing;
`PreparedMultiPathEdit` also stages the complete group first. Publication is
atomic per file, not across the group. A partial failure must identify the
already committed paths through the typed tool receipt, including LSP edits.
Journal after-state, read evidence and displayed diffs
must describe the bytes actually committed, not an implicit later rewrite.

## Cloud workspace boundary

The current implementation has no production `gateway_relay` or
`sandbox_resident_agent` execution adapter. These bindings retain explicit route
metadata and fail with `transport_unavailable`; they never fall back to
Server-local execution. A future provider must use the canonical owned
invocation boundary and supply real enforcement evidence.

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

Filesystem path authorization is owned by `astra-sandbox`. In Standard and Strict
isolation, relative inputs must stay within the selected workspace after resolving
symlinks and parent components, including nonexistent tails. Additional directory
grants apply to explicit absolute inputs; they cannot authorize relative traversal.
CLI and shared filesystem tools use this same resolver. Explicit Permissive mode
retains its broader authority and bypass-immune credential exclusions.

### Workspace observation coordination

Local shell and typed workspace writers must serialize each executor-owned
pre/execute/post observation window across Astra processes without placing the
coordination authority inside the tool-writable workspace.

The CLI checks this host prerequisite before submitting a turn that advertises
locally bound workspace writers or shell tools (including deferred tools).
Text-only and read-only surfaces do not require it. Unsupported hosts fail with
an actionable local setup error before model execution; admission also rechecks
the same trust boundary. Tool-level refusals use the non-retryable
`workspace_host_unsupported` kind, distinct from a missing workspace binding.
A CLI preflight refusal preserves `error_kind=tool_unavailable`,
`error_code=workspace_host_unsupported`, non-retryable metadata, exit status `1`,
and `server_admission_not_dispatched=true` in terminal JSON. It creates no Run,
execution journal entry, accounting reconciliation, or usage publication. Human
output retains the stable code and administrator guidance on stderr even when
stdout uses JSON or stream-json. Interactive refusal uses the existing
not-started settlement: the draft, recovery guidance and tool restrictions remain
available for a later attempt.

Rebinding a workspace or changing cache roots cannot repair a host whose
coordination directory is not trusted; use a supported host or have its
administrator provision the required root-owned sticky directory.

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
- macOS watcher admission atomically reserves the kqueue descriptor and every
  duplicated witness or opened binding descriptor before opening any watcher
  files. The process-wide budget is the smaller of one quarter of the current
  `RLIMIT_NOFILE` soft limit and the soft limit minus 64 descriptors of
  headroom. Watched paths consume that same budget. Transient capacity shortage
  releases admission locks and waits
  within the caller's remaining monotonic deadline. Exhaustion returns typed
  `WorkspaceLeaseFailure::WatcherCapacity` facts with active/requested/limit
  counts, carried into tool result fields as `reason: watcher_capacity` and
  `watcher_descriptors`. These are the last observed capacity counts. If a retry
  uses the remaining deadline before reaching watcher admission, expiry retains
  that observed refusal instead of reclassifying it as workspace contention.
  A definitive cancellation, binding failure, or ownership failure still wins.
  It does not instruct the caller to wait for an operation
  in this same workspace. A zero limit or a watcher larger than the budget
  rejects immediately with `retryable: false` and a resource repair action.
  Cancellation, failed registration, and lease drop
  close descriptors before releasing the reservation. The budget covers
  watcher-owned descriptors, not all runtime files/sockets; unrelated resource
  exhaustion still fails closed. Lowering the soft limit does not revoke
  existing leases, but subsequent admission uses the new limit.
- Kernel ownership must end automatically when the holder process exits.
  Replacing or unlinking a witness or workspace binding must not admit a second
  generation, and any observed tamper revokes receipt authority permanently
  for the active lease.
- Platforms without an equivalent trusted namespace and tamper watch fail
  closed before launching a local command.

Workspace lease admission uses the asynchronous options APIs with explicit
cancellation and wait budgets. Observation, mutation, and opaque writer APIs
return `Result<_, WorkspaceLeaseFailure>` with typed admission failures. The unused synchronous acquisition APIs and
no-options observation wrapper are retired from the Rust source API. Process
coordination, generation watches, diagnostic paths, and guard cleanup remain
shared; tests exercise the same acquisition paths as executors.

Native collaborator delivery delegates workspace serialization to the CLI tool
executor. It does not hold a workspace lease while asking for approval or acquire
the executor's lease a second time. After acquiring the lease, the executor checks
the current permission attachment, workspace and executable identity before
dispatch; queueing cannot preserve revoked authority. Lease waits respect the
invocation deadline and cancellation. The Linux CI composition test drives the
authenticated WebSocket owner through this executor and the production process
supervisor, including contention, cancellation, expiry and changed admission.

Native discovery's physical process lifetime covers its complete caller budget;
handshake/authentication and model-list waits cannot renew that cutoff. A typed
collaborator uses its admitted whole-stage deadline when present; absence of a
work deadline does not install a command-timeout fallback. Initialization and
authentication remain bounded control exchanges, independently of the active
turn and each command's limit. Physical Edge detach cancels and settles owned
execution before another attachment serves work; its journal retains results
for reconnect replay. This is separate from a client UI disconnect, which does
not revoke the Server's durable execution authority.
An explicit model request cannot be replaced by a provider default merely because
discovery is unavailable; only unconstrained or explicitly accepted defaults may
omit the selector.

Linux runtime authorization remains canonical. The native sparse filesystem
also exposes a platform bootstrap alias only when its current resolved target
is in the approved runtime requirements. Sensitive-path rules cover both views;
Codex's explicit minimal bootstrap preserves the logical loader paths only when
all existing platform runtime roots are approved. Its additional `/etc` and
inherited `/proc` views remain denied; filesystem-root read is never granted.

Receipt attribution and execution coordination are independent. A completed
foreground process group may be too weak to authorize future fingerprint-based
receipts: an escaped descendant could write later. That uncertainty quarantines
receipt attribution, but does not by itself revoke a healthy coordination lease
or prevent the next command. Unsettled execution ownership and a replaced or
tampered binding still prevent admission. Every receipt-producing path must
check receipt authority independently of the coordination check used to launch.

Receipt-attribution quarantine is retained per workspace observation root for
the executor process lifetime, not per conversation. It prevents new attribution
receipts, while healthy execution coordination remains independent. Explicit
verification records the observed command result and does not require a
fingerprint. Quarantine diagnostics log a root digest, not local paths or shell
input. A new conversation does not clear quarantine; before restarting
the affected Astra or astra-edge process, verify that earlier background work
has stopped. Restart alone does not stop escaped descendants or prove safety.

### Prepared directory inspection

Prepared Unix Bash invocations inspect targets through the same retained
workspace and working-directory handles used for execution. Directory acquisition
walks each component without following symlinks. Target inspection resolves
symlinks with bounded traversal and checks directory ancestry by identity; it
does not assume that an existing `/dev/fd` entry supports child-path lookup.
On Linux, traversal and identity handles use `O_PATH`, so acquiring the filesystem
root or workspace ancestors does not require directory-read permission from
Landlock. Actual file reads and directory enumeration still open separate readable
handles and remain subject to the selected provider's filesystem allowlist.
The OS sandbox remains responsible for execution-time filesystem confinement.

Source preimages read retained regular-file handles with a bounded byte budget.
They retain the captured parent identity in the receipt: replacing a parent must
not turn another file into a valid post-image or restore destination. Restore
checks the workspace binding and opens targets relative to retained directories.
Working-directory names are verified display labels, not IO authority; an
unavailable label is reported as unknown rather than silently as the root.

## Result boundary

Tool-output governance preserves retained plaintext segments and decoded JSON
string content, including line endings and whitespace. It removes injection lines
and redacts credentials; JSON serialization may normalize the surrounding format. Reapplying governance must preserve the same governed result; display
projections cannot replace full journal evidence. CLI and Server journal records
use the same governed content without UI-only diff attachments; the original
callback and local UI output remain separate.

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
owner under the workspace writer lease. `enter` creates and selects a new
worktree; it does not adopt a separately created worktree. Bash `workdir` selects
only one invocation's directory. Local CLI/User Runner calls can use an existing
external directory under their current sandbox policy, including already granted
temporary worktrees; the selected session workspace and command permissions stay
unchanged. Restrictive relative paths remain workspace-bound; absolute paths may
use explicit existing grants. Permissive mode retains sensitive-path exclusions.
Generic Server execution remains workspace-only. The pinned directory is checked
against its current authorization before launch, and relative read registration
uses its verified identity. A pre-start refusal remains `rejected` / “Did not run”
through callbacks and transcript restoration; it is distinct from an attempted
command that failed. Bash sandbox retries use the producer's local
`SandboxApprovalScope` handle for the exact existing directory. The
`sandbox_approval_directory` callback field is diagnostic, not grant authority.
Ambiguous or unrepresentable targets are returned to the model without guessing
an approval scope from error prose.
Callback run/call scope remains attached through root auditing. Resume joins
existing callback facts with transcript items by that exact identity. If a
remote transcript omits the tool row, confirmed refusals are rendered from the
existing scoped audit fact. This changes neither provider messages nor durable
projections and adds no SQL reads.
It has no legacy `git(action=worktree)` alias. Ordinary worktree Git commands remain shell operations. Generic Server
executors cannot silently create a local session owner. Typed commit/stash tool
journals and their automatic compensation are removed along with their producers;
shell side effects retain the existing shell observation and recovery contract.
Ordinary shell commands return their execution outcome without scanning the
workspace before and after execution. A changed snapshot cannot identify the
writer when another session or editor is active; only explicitly requested
verification uses a bounded workspace comparison.

The session-selected worktree applies to file, search and shell tools across
turns and local resume. If the saved checkout no longer belongs to the original
repository, local tools wait for the user to restore it or explicitly leave it;
they do not silently run in a different directory. `start_point` can select an existing commit or ref; the new branch is
created at its resolved commit. Switching is rejected while workspace work is
still active, and removal without `discard_changes` also preserves ignored files.
Background work uses the existing invocation owner. An executor that cannot
prove descendant settlement rejects managed background work before starting it;
this currently includes macOS. If a started process cannot be settled, the
workspace remains fenced. Background stdout and stderr are each capped at
4 MiB and marked when truncated. A checkout is retained when its owner is
known to be unsettled.
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

On macOS, a Git checkout hook can leave a process behind even before Astra runs
a command in the new worktree. Exiting therefore preserves the checkout and
restores the original workspace. After checking for processes, the user may
remove it with Git. A genuinely unsettled owner keeps the affected workspace
fenced; after checking that its work stopped, the user can confirm and retry in
the TUI or use `astra workspace confirm-stopped` from that workspace. A human
confirmation is an explicit recovery decision, not proof that unrelated
external processes stopped.

Before a local tool chain starts, the shared permission engine evaluates each
step using its resolved input. Steps requiring individual approval or depending
on a previous result must be called separately, so a chain cannot bypass a
tool-specific denial or approve an action whose arguments are not yet known.
Failed or cancelled chains and plan subtasks retain effects already produced,
stop remaining dependent work, and report the failure. Automatic filesystem
rollback cannot cover arbitrary tools and might overwrite later edits, so it
is not a chain or batch guarantee. An explicit file undo checks the recorded
post-state and reports conflicts or partial results; it is not atomic against
concurrent writes from external programs.

When the Linux invocation supervisor is used for Bash, its subreaper settles
its child ownership only after kernel
`waitpid(-1, WNOHANG)` reports `ECHILD`. Enumerating its own proc children is an
auxiliary means to kill adopted descendants. If that enumeration is unavailable,
the same bounded loop continues waiting and reaping under its original deadline.
Neither an unavailable or empty scan nor deadline expiry proves settlement; a
live child at expiry or an unexpected wait error retains unsettled ownership.
The helper's non-dumpable boundary and nonce-bound terminal receipt apply to
that path; Server run settlement cannot replace this local scope proof.

Other Bash paths may execute directly in a cgroup or process group without the
supervisor; `run_script` retains its cgroup prerequisite. Group cleanup cannot
prove that a descendant which created a new session has stopped, and a same-user
process may be able to move between delegated cgroups. When a script explicitly
requests memory or CPU limits, it is rejected before starting if those limits
cannot be enforced.
`run_script` does not itself add filesystem or network isolation.

The optional asynchronous `check-ignore` adapter retains startup binding checks;
it does not inherit synchronous invocation ownership or post-execution binding
validation. Its input/output exchange runs concurrently under one five-second
deadline after spawn, and cancellation drops the kill-on-drop direct child.
Synchronous repository validation precedes that exchange.

## Shared search execution

CLI grep applies its current path policy, including approved additional paths,
before invoking the shared search implementation. An authorized external target
does not become the workspace root or acquire workspace-observation authority.
Independent User Runner executors continue resolving targets through their own
workspace boundary. Both paths share search arguments, ignore rules, pagination,
bounded output, timeout and caller-owned cancellation. The CLI supplies its
pressure-scaled output budget through the existing execution context. It preserves source
error and exit metadata instead of reinterpreting result text.

When the search fallback must enumerate files, it bounds the candidate list
at 8 MiB. A larger or incomplete listing returns an actionable error instead
of searching a truncated list; narrowing the path or using ripgrep can continue
the search. Output truncation never turns an incomplete filename into a result.

## Path sensitivity and ordinary project content

A hidden directory name under the user's home is not itself a sensitive-state
boundary. Ordinary project content remains editable there. Permission admission
continues to classify known credential files, authority metadata and
owned internal artifacts through the shared path-sensitivity owner; sandbox and
explicit workspace authority still apply independently.

IDE settings, shell configuration, `.gitmodules` and other ordinary project
configuration follow the same workspace write authority as source files. Merely
being configuration or potentially executable does not create a second approval
boundary. Git/agent authority metadata and credential rules remain separate.

Local Bash uses the selected workspace for session ownership and the explicit
`workdir` for one command. An external cwd must already be authorized by the
existing sandbox policy, or receive the existing sandbox expansion approval.
Bash has no separate read-only cwd preflight. A confirmed pre-start denial may
carry a local handle for the exact existing approval directory; callback JSON
is diagnostic and cannot supply that authority. A missing or replaced approval
directory fails closed, without selecting an ancestor or retrying the command.
Permission checks use frozen canonical grant prefixes. Restoring a policy does
not resolve saved prefixes into new grants; old noncanonical aliases may fail
closed. Grants retain the existing path-permission semantics after commit.
Standard isolation does not provide OS confinement against arbitrary filesystem
mutation after its final check. No execution lifecycle, journal lane or SQL
operation is added by this separation.

Cloud approval denial uses the existing `approval_denied` identity and preserves
its supplied reason. A denied decision proves that the gated tool did not run;
it does not establish whether a user, policy or unavailable interface caused
the decision. The model-facing projection therefore adds no user attribution
or inferred retry instructions. Queue failures retain their existing typed
busy/unavailable cause through the scoped callback and the same projection.
