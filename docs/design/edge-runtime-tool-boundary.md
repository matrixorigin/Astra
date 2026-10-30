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

### Foreground Bash output

The local CLI and Edge foreground Bash executors share the output-capture owner
without replacing their existing process-settlement or workspace-lease owners.
They keep stdout and stderr pipes as the command's own stdout and stderr,
and capture their original bytes in
separate `0600` files with a shared random 16-lowercase-hex basename and
`.stdout` / `.stderr` suffixes under the selected workspace's
`astra-bash-outputs/` directory. This dedicated top-level directory holds
ordinary user-readable output data, not Astra's private `.astra/` state; it is
excluded from executor workspace-mutation fingerprints without excluding
other user paths. Git-backed observation excludes that exact directory before
its bounded status probe, so retained outputs do not consume the mutation
observer's byte or entry budget. This is a tap after the top-level Bash
process, so shell pipelines and explicit redirection keep their normal
behavior. The file-retention threshold is the lesser of the configured
per-tool byte limit and the shared 8,000-character model inline budget. The
formatted, redacted output plus its actual exit note determines retention;
small results remain inline even with a low configured budget. Stream-preview
capping also retains the files. Byte counting is conservative for UTF-8.
Retained results return both
file paths, byte counts and SHA-256 digests. The redacted preview uses space
remaining after the receipt and terminal facts in a 4,000-byte presentation
target; an exceptionally long workspace path can make the mandatory receipt
itself exceed that target. The configured limit bounds disposable preview,
not the mandatory file receipt or terminal facts; even a 64-byte budget must
return complete paths, byte counts, digests and completeness. The owner marks
this bounded native projection so downstream recording does not truncate its
recovery receipt a second time. A complete capture's receipt instructs the caller
to copy its paths exactly and filter or format the saved files locally,
emitting only the fields or sections needed for the answer instead of
rerunning the original command merely to change presentation. This guidance
precedes the preview and consumes the same inline budget; incomplete captures
do not receive it. File names are ordinary workspace paths, not authority or
aliases; exclusive creation rejects a collision without overwriting a file.
The inline view remains bounded and applies
the existing display redaction; the files deliberately contain raw bytes and
must be treated as ordinary user workspace data, not redacted or immutable
session artifacts. The digest describes bytes observed when the receipt was
created, not a guarantee against subsequent same-user modification.

The combined capture hard limit is 50 MiB per invocation. Timeout,
cancellation, stream failure or the capture limit yields an explicit
`complete=false` result and never claims that partial files hold the full
output. A post-execution capture failure preserves the command's exit and
process-settlement facts. Capture files follow the selected workspace's
lifetime: an ephemeral workspace disappears with its existing lease; a local
workspace retains large-output files like any other Bash-created file. There
is no independent output-file retention service.

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
observed results and usage, reports a typed protocol failure, and uses the
existing physical-run cleanup path for an unsettled owner. It must not replay
tools or open another admission to complete an incomplete stream.

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
Inspection and diagnostics omit `mode`. Explicit `mode=verify` adds an unchanged-workspace proof obligation
after edits. If bounded observation is unavailable, command output is retained
but the requested verification fails closed; it must not mint a completion or
no-change receipt. Command text does not select or weaken this contract.

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
  files. The process-wide budget is the minimum of 1,024 descriptors, one
  quarter of the current `RLIMIT_NOFILE` soft limit, and the soft limit minus
  64 descriptors of headroom. Watched paths consume that same budget; there is
  no unbounded path subscription pool. The 1,024 ceiling normally binds on
  hosts with a soft limit of at least 4,096; the other terms protect low-limit
  processes. Transient capacity shortage releases admission locks and waits
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
verification still fails when the required proof is unavailable. Quarantine
diagnostics log a root digest, not local paths or shell input. A new conversation does not clear quarantine; before restarting
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

On Linux, the invocation subreaper settles ownership only after kernel
`waitpid(-1, WNOHANG)` reports `ECHILD`. Enumerating its own proc children is an
auxiliary means to kill adopted descendants. If that enumeration is unavailable,
the same bounded loop continues waiting and reaping under its original deadline.
Neither an unavailable or empty scan nor deadline expiry proves settlement; a
live child at expiry or an unexpected wait error retains unsettled ownership.
The helper's non-dumpable boundary and nonce-bound terminal receipt remain
required, and Server run settlement cannot replace this local scope proof.

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
