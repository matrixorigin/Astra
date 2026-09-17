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
perform an O(n²) renewal of every earlier chunk. Sealing atomically replaces
the temporary edges with ordered artifact-to-content references. The first
workspace package uses one typed artifact containing the snapshot manifest plus
deduplicated file bytes, rather than one catalog row per file.

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

## Tool output handling

Large or unsafe tool output should be stored as artifact and summarized through the tool result quality firewall.

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
