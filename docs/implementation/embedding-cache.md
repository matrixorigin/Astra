# Skill Embedding Cache

> **Last Updated**: 2026-02-21

How skill embeddings are managed and when caching becomes necessary.

## Current Behavior

`ModernSkillSelector` embeds all skill descriptions at construction time via `embed_fn`. The `SkillPipeline` is a long-lived object in `ChatLoop`, so embeddings are computed once per process lifetime.

**Current scale**: <20 skills → 6 API calls at startup → negligible cost and latency.

## When Caching Is Needed

- Skill count > 20 (startup cost becomes noticeable)
- Frequent pipeline reconstruction (e.g., per-request in serverless)
- Multiple processes not sharing embeddings

## Caching Strategy

When needed, add a content-hash-based LRU cache:

```python
# Cache key: (skill_name, skill_version, content_hash)
# Content hash: SHA256 of name + description + triggers
# Invalidation: automatic when skill content changes
```

Options by deployment model:

| Deployment | Cache Layer | Sharing |
|-----------|-------------|---------|
| Single process | In-memory dict | Process-local |
| Multi-process | Redis | Cross-process |
| Serverless | Redis + warm start | Cross-invocation |

## Current Status

Not implemented — unnecessary at current scale. The `SkillPipeline` singleton pattern in `ChatLoop` means embeddings are computed once and reused for the lifetime of the process.
