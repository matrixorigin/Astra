# EmbeddingClient Refactoring Plan

## Problem
EmbeddingService has runtime fallback (OpenAI->local->mock) that silently mixes incompatible vectors in the same DB column. Dimension hardcoded to 1536 with zero-padding.

## Solution
Explicit config, no fallback, configurable dimension. See conversation for full details.

## Tasks
1. Settings + EmbeddingClient skeleton + MockProvider
2. LocalProvider (sentence-transformers)
3. OpenAIProvider
4. Configurable VectorType dimension
5. Replace all EmbeddingService callers
6. .env files + startup validation
