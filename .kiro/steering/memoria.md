---
inclusion: always
---

# Memoria Integration Guide

Memoria is treated as an external memory service from this repository's perspective.

## Current Integration Surface

- configure `MEMORIA_BASE_URL`
- optionally configure `MEMORIA_MASTER_KEY`
- API-shell forwards memory-related requests through its app-state wiring and HTTP handlers

## Repository Expectation

Do not assume a checked-in FastAPI Memoria application is the primary local development path for this repo.
Focus on the Rust integration points that call Memoria, validate request shaping, and preserve session-aware forwarding behavior.

## Validation

If a change touches Memoria integration, run the relevant API-shell Rust contract tests plus the usual static checks.
