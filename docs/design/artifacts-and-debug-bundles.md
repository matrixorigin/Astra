# Artifacts and debug bundles

> Status: target design contract; the shared byte-content backend is now
> implemented for immutable artifacts, while Work capture/upload and restore
> clients remain a later vertical slice.
> Last updated: 2026-09-17.

Artifacts and debug bundles define how Astra stores large outputs, raw captures, manifests, and support diagnostics without polluting normal trace or learning data.

## Artifact classes

| Class | Purpose |
| --- | --- |
| User-visible artifact | Generated file, report, patch, exported result. |
| Tool artifact | Large or binary tool output referenced by summary. |
| Trace artifact | Structured supporting evidence for replay/debug. |
| Debug bundle | Explicit raw diagnostic capture with short TTL. |
| Learning artifact | Redacted, consent-gated derived data. |

## Manifest

Every artifact should have a manifest:

```text
artifact_id
session_id
run_id
kind
content_type
size
hash
retention_policy
redaction_status
visibility
source_event_refs
created_at
expires_at
```

Immutable binary artifacts use the same catalog, owner, reference, retention,
and audit boundary as JSON artifacts. Their content is stored as
owner-scoped, content-addressed chunks and sealed only after the server has
verified every digest, size, order, and aggregate hash. A chunk digest is an
address within the authenticated owner scope; it is never a download grant.
An unfinished upload has one durable artifact-level lease, refreshed by each
successful chunk put, plus a temporary reservation edge for every chunk. The
lease is the only upload lifetime authority; the edges describe which chunks
must remain reachable. Retention GC fences the lease before removing its edges,
so a reused chunk cannot be collected between puts and seal, and uploads do not
perform an O(n²) renewal of every earlier chunk. Seal returns its already verified bytes and catalog from the same transaction,
including exact replay; the HTTP response verifies that bundle without
loading the full content again. Sealing atomically replaces
the temporary edges with ordered artifact-to-content references. The first
workspace package uses one typed artifact containing the snapshot manifest plus
deduplicated file bytes, rather than one catalog row per file. The content
store admits at most 64 MiB of logical chunk bytes, 4,096 chunks and 16 MiB per
chunk. Repeated chunk positions count toward the byte budget; a workspace
package uploads each unique blob once, so its budget is the deduplicated blob
sum, not the sum of every file path's size. Begin, upload, seal and verified
reads enforce the same limits, including recovery publication. Shared chunks
are read in batches of up to 128 unique digests while preserving global
owner/digest lock order and the existing GC fencing. Stored sizes must match the admitted plan
before bytes leave the database. Package validation borrows these bytes and
hashes each unique blob once per validation; it does not retain a validation
cache across authorization or transaction boundaries.

## Tool-output previews

Tool-output previews use the synchronous policy in `services/src/runs.rs`:
36 explicit tool mappings take precedence over the existing built-in tool
registry's generic 1,200-byte `text_v1` policy. Unknown names use a 400-byte
`raw_v1` fallback. Normalization versions label persisted previews; they do
not select a normalization algorithm. UTF-8 truncation, serialized-payload
hashes, explicit artifact references, and parent-output identity are preserved.
Fallback persistence does not emit missing-template events or change session
event counters. The output transaction itself takes the canonical session
write fence, requires an existing session, and rejects deletion-fenced writes.

Fresh schema contract `2026-09-22-v88` removes the two static SQL registries
without migration or import of SQL overrides. Raw-reference scheme membership
is not an authorization authority: concrete artifact readers continue to own
session ACL and content-integrity checks.

New tool-result artifacts admit up to 16 MiB per document. The turn journal
records a small receipt with the observed byte count when its document
exceeds the limit. The receipt preserves the execution outcome and carries
no artifact handle. Oversized runtime
guidance is omitted independently of the tool result. If the guidance
artifact cannot be written, the journal retains the bounded inline preview.
Governed result writes return the committed artifact ID directly, avoiding
the post-commit query that would reload the body and its reference count.

## Debug bundle rules

Debug bundles are off by default.

Requirements:

- explicit enablement;
- short TTL;
- manifest;
- access audit;
- delete/export operations;
- redaction boundary;
- exclusion from default learning pipeline.

CLI context dumps use `astra.context_dump/v3`. They retain the recorded prompt
trace, conversation, usage and compaction facts; the empty local built-in skill
selection field is retired. Actual skill injection remains in the trace.

## Tool output handling

Large or unsafe tool output should be stored as artifact and summarized through the tool result quality firewall.

The immutable tool-result artifact preserves the exact UTF-8 bytes after the
existing safety/redaction boundary and before lossy model presentation. Storage
does not pretty-print or otherwise rewrite those bytes: descriptor hashes,
byte ranges and recovery windows must all address the same source. Formatting
belongs to bounded display projections.

The model-facing tool-result reference is `artifact://session/tool-result/result_<sha256>`
(or `guidance_<sha256>` for the separately identified runtime-guidance document).
The digest addresses a small immutable descriptor manifest in the same
owner/session-local tool-result store. It covers the producer run, call,
document kind, byte length and content digest. The reader verifies the manifest
and then verifies the original payload through the existing bounded reader.
Serialized manifests have a 64 KiB metadata bound, enforced before publication
and by a bounded read before hashing or decoding; payload paging limits remain
independent. Immutable replay requires a regular file for both payloads and
manifests, and rejects symlinks even when their target has the expected bytes.
Missing, non-regular, oversized or changed manifests fail explicitly; references never
select a nearby result or cross owner/session boundaries. Manifest publication
is required before a successful recoverable projection is returned. Exact
replay may finish publication after an interrupted write. Existing inline
descriptor references in durable histories remain readable and preserve their
own reference while paging; reading does not create a replacement manifest.

Artifact possession does not grant read authority. Stored result previews and
compact projections retain the artifact identity without promising a callable
reader. The shared request assembler derives bounded recovery guidance from
the current direct/deferred tool surface and runtime restrictions. This guidance
does not rewrite result bodies, widen child permissions, or replace semantic
introspection and reflection. Native tool pagination retains its own contract.

Artifact retention and reachability are separate facts:

- retention policy defines when otherwise-unreachable content becomes eligible
  for reclamation;
- a durable reference blocks reclamation but never manufactures a later policy
  deadline;
- every reference has an owner kind and owner identity and supports bounded
  forward and reverse lookup;
- ownership transfer and deletion of the source record occur in one durable
  transaction;
- a sweeper releases references only when the owning evidence expires, then lets
  ordinary retention GC decide whether the artifact can be reclaimed;
- large-result persistence failure is explicit durability failure, never
  success with an unusable or missing artifact reference.

## Test obligations

- Large tool output does not enter prompt raw.
- Large tool output cannot be recorded as complete success unless its required
  artifact is durable.
- Invocation-to-archive compaction preserves result-artifact reachability and
  archive expiry releases it.
- A durable reference blocks collection without extending the configured
  retention deadline.
- Forward and reverse reference lookup agree on owner and artifact identity.
- Debug bundle expires or is deletable.
- Artifact access is auditable.
- Learning pipeline cannot consume C4 debug bundle without opt-in.

## Debug bundle payload classes

A debug bundle may include only explicitly enabled classes:

| Class | Examples |
| --- | --- |
| prompt_capture | rendered prompt or prompt hashes. |
| model_capture | request/response metadata, provider ids, token usage. |
| tool_capture | raw or redacted tool inputs/outputs. |
| provider_capture | provider decisions, offline/degraded reasons. |
| sync_capture | outbox state, poison summaries, ack watermark. |
| ui_capture | stream cursor and projection summaries. |

Each class has independent redaction and retention policy.

## Debug bundle test obligations

- Bundle creation records consent and audit event.
- Bundle manifest lists payload classes and redaction status.
- Expired bundle is inaccessible except allowed audit metadata.
- Export does not include classes that were not enabled.
- Deletion propagates to artifact references and learning exclusions.

The v3 context dump uses the same typed `ContextAssemblyTrace` payload in live
and journal `trace` fields. An absent capture produces `trace: null`; the latest malformed
or unsupported journal trace fails the dump explicitly. It does not fall back
to an older trace or convert retired compression-detail shapes. Visible chat
history is separate display evidence, not an exact provider-request replay.

The v3 trace also retires the generic explanation array: it had no production
producer. Canonical memory/tool exclusions, provider decisions and durable
execution evidence remain available through their existing owners.
