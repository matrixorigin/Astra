---
name: jev-memory
description: Reproduce Astra's No-JEV, JEV and DeepSeek Flash JEV-like memory-injection component experiment, including relevance, answer quality, latency and cache-aware cost analysis.
---

# JEV memory-injection experiment

This directory packages the experiment resources. It uses the real Rust implementation in the containing Astra repository, not a duplicate selector. Read [references/protocol.md](references/protocol.md) first for the measurement scope, three comparison arms, failure behavior, pricing, and truncation limits.

## One-command entrypoint

Run from the repository root; the entrypoint also resolves its repository when invoked from another directory:

```sh
# Default: offline checks, without loading credentials or calling models
python3 expriment/jev-memory/scripts/run.py

# Explicitly authorized paid run: four suites, three repeats, three arms
python3 expriment/jev-memory/scripts/run.py --live --models /absolute/path/to/models.yaml
```

Prerequisites: Python 3 with its standard library, the repository-pinned Rust toolchain, and downloaded Cargo dependencies. Builds use `--offline`. Prepare missing dependencies using the repository's development instructions; dependency-resolution failures are not model-quality failures.

`--models` must reference a user-provided local YAML file whose top level is a list, without a `models:` wrapper. It must contain `jev-1.13.0` and `deepseek-v4-flash`, each with `name`, `provider`, `base_url`, and a nonempty literal `api_key`. The user can copy [fixtures/models.yaml.example](fixtures/models.yaml.example) outside the repository, fill in their keys, and restrict file permissions. Never pass keys on the command line, print or automatically copy credentials, or import/change the user's model registry. This harness does not expand environment-variable placeholders in YAML values.

`--live` explicitly selects paid provider calls. Do not enable it for a request limited to inspection, explanation, or offline reproduction. The default full run produces 432 rows and 720 logical model calls; physical adapter retries may incur additional charges. Use `--suite main|regression|scale|recall` or `--repeat 1..5` for a smaller run, and disclose the reduced scope in its report.

## Execution and artifacts

The entrypoint runs the Python offline tests, shared judgment-contract tests, and runtime memory tests, builds the real harness, then invokes its test binary directly. Do not compile or run competing load tests during measurement. Invoke only the designated paid test, never the entire ignored-test suite.

Each invocation creates a fresh `target/expriment/jev-memory-*` directory containing generated scale fixtures, raw suite records, `summary-official.json`, and structured request/response `evidence.json`. Retain failures rather than selectively rerunning them. Script errors stop the run; preserve incomplete records and use a new directory after a repair. Raw output directories have private permissions. Publish evidence only after reviewing it for sensitive content: the exporter allowlists fields but does not scrub personal information from custom fixtures.

The default `fixtures/memory-article-prices-20260920.json` reproduces the historical pricing basis; it is not a current-price guarantee. For current estimates, verify official rates and supply a new `--price-card /path/to/card.json` recording its date, currency, peak/off-peak tier, and cache rates. Do not change the historical card. Missing usage is not zero cost.

`results/2026-09-20/` retains the article's evidence, including failures and earlier implementations. New runs never overwrite it. Historical hashes and paths are original provenance; do not rewrite them after moving files. Remote aliases, cache state, and service load can change results. Reproducing the protocol does not guarantee identical outputs or measurements.

## Reporting and boundaries

Report selection accuracy, precision/recall, downstream answers, auxiliary/total P50 and P95, failures and fallbacks, cache usage, and auxiliary/total costs separately. Keep the four truncation-pressure cases separate from the core headline. The scale suite contains only two task families; its sample-maximum P95 is not an SLA estimate. Post-diagnosis regressions are not blind evaluations.

JEV-like denotes the shared judgment contract and the observe–judge–act–verify design pattern, not merely substitution of a cheaper model. This experiment measures one context-selection step and one downstream answer; it cannot establish savings across a complete agent's tool-use loop. The primary model, executor, and permission boundaries retain their existing responsibilities.

Keep experiment documentation, script comments, and CLI help in English. Preserve multilingual fixtures and recorded responses unchanged: their language is part of the measured input/output, not documentation to translate.

Write the accompanying article in Chinese and deliver it outside the repository, at the user's chosen path (`~/jev-memory-evaluation-2026-09-20.md` for this article). Do not move it back into `docs/`. To use this skill in an agent, provide this `SKILL.md` path explicitly; placing it in a custom repository directory does not automatically install it in a global skill directory.
