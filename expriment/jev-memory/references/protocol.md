# Live memory-injection component evaluation

This opt-in, paid harness supports the JEV integration article. It exercises the
real Rust `select_memories` implementation, strict judgment codec, lexical/no-dismissal
fallbacks, and nonstream provider adapters. It is not a Server/DB/Offering-admission
test, Memoria retrieval benchmark, or full tool-using Agent evaluation.

See the [published results index](../results/README.md) for the retained final
measurements, optimization baselines, and their complete structured evidence.

## Fixed comparison

Three arms share the same synthetic candidate list and task:

- `no_jev`: no auxiliary model; production lexical relevance / no-dismissal fallback.
- `jev`: `jev-1.13.0` using the native TypeSafe adapter.
- `flash_jev_like`: `deepseek-v4-flash` using the shared typed judgment messages.

All downstream answers use `deepseek-v4-flash`, thinking off, temperature zero,
256 output tokens, and a 20-second deadline. Auxiliary calls use the memory
owner's unchanged three-second deadline and dynamic ID-based output allowance.
The harness does not add retries; the real provider adapter owns its retry policy.
Rows count logical calls, not a separately instrumented physical-attempt ledger.

The downstream prompt uses the product's Session Lessons heading but a small,
controlled JSON answer task, not the full production prompt assembler. Relevance
tasks receive the selected memories. Dismissal tasks start with all six lessons
injected, remove selected rejected lessons, retain the user's correction in the
transcript, then ask a follow-up question. No persistent memory is deleted.

Exact selection is order-independent. Predeclared answer fields are checked
deterministically; there is no model grader and no claim to measure all aspects
of answer quality. Selection and answer scores are separate. Labels were authored
for these cases, not independently double-blind annotated. Three repeats are
stability observations, not three times as many independent scenarios.

## Run

Use an ignored local model YAML containing the two exact names above. Never pass
provider keys as command-line arguments. The current harness uses literal keys
from that file; it does not seed a registry or change deployment bindings.

Build and run offline checks **before** latency measurement. Do not compile or
run other load-generating benchmarks while the live evaluation is running.

```sh
cargo test -p astra-turn-types judgment::tests --offline
cargo test -p astra-runtime --lib --no-default-features \
  --features live-provider-tests memory_hooks --offline -- --test-threads=1
python3 -m unittest discover -s expriment/jev-memory/scripts -p test_analyze_memory_article.py

ASTRA_MEMORY_EVAL_CASES=expriment/jev-memory/fixtures/memory-injection-article-cases.json \
ASTRA_MEMORY_EVAL_MODELS=.models.yaml \
ASTRA_MEMORY_EVAL_OUTPUT=target/memory-article-new-run \
ASTRA_MEMORY_EVAL_REPEAT=3 \
cargo test -p astra-runtime --lib --no-default-features \
  --features live-provider-tests \
  memory_hooks::article_eval::memory_injection_article_live \
  --offline -- --ignored --exact --nocapture --test-threads=1
```

The output directory must not already exist. It is created with mode 0700 on
Unix. It contains a manifest (commit, source and binary hashes, model identity,
input hash, budgets, prices), frozen cases/source, flushed per-case JSONL, and a
completion marker. Failures remain observations, not silently retried cases.
The test completing means collection completed, **not** that model scores passed.
Do not run the ordinary ignored-test suite to invoke paid tests accidentally.

The fixed core suite has 20 ordinary cases and four truncation-pressure cases.
Run `expriment/jev-memory/fixtures/memory-injection-article-regression.json` separately for the eight
additional post-diagnosis regression scenarios; it is not a blind holdout.

The eight `fixtures/memory-injection-recall.json` scenarios were frozen before
the contribution-policy optimization. They cover partial answers, multiple
independent facts, applicable procedures, missing information, scope mismatch,
explicit exclusions and current-request overrides. An unchanged-policy baseline
was collected before applying the optimization. They are developer-authored
generalization checks, not independently blinded examples. The one-command
runner includes this suite as `recall`; use `--suite recall` to run it alone.

The contribution policy changes only the shared relevance instruction and
positive/negative criteria. It does not lower the threshold, add another model
call, increase input truncation limits or change dismissal behavior. Report the
added input tokens and costs as well as precision, recall and downstream results.

The runner exports `evidence.json` for all cases and repetitions, including
deduplicated typed requests, actual provider answers and native probabilities.
Each request stores its original serialized `raw` string; parse it as JSON when
needed, and use the string's UTF-8 bytes to verify its SHA-256 reference.
Match its raw-results hash and provenance with `summary-official.json`. The
export excludes environment and model config; review synthetic inputs before
publishing. It is not a sanitizer for arbitrary private datasets.

For same-fixture comparisons, run:

```sh
python3 expriment/jev-memory/scripts/compare_results.py BEFORE.json AFTER.json --group core
python3 expriment/jev-memory/scripts/threshold_sweep.py BEFORE-evidence.json
```

The comparison rejects mismatched fixture hashes, repeat/deadline settings and
prices. The threshold sweep is **post-hoc selection-only analysis** of native
probabilities; it does not replay answers, establish calibration or validate a
threshold for new workloads. It must not be reported as a fresh live success rate.

## Candidate-count pressure

```sh
python3 expriment/jev-memory/scripts/memory_article_scale_cases.py \
  --output target/memory-article-scale-cases.json
```

Use the generated file as `ASTRA_MEMORY_EVAL_CASES` with a fresh output directory.
It contains two task families at 6, 24, 96, and 256 candidates. Exactly two useful
memories remain at fixed relative positions while distinct hard negatives grow.
Every query/candidate fits the production truncation window, isolating batch
size from information loss. Size order is non-monotonic; three-arm order rotates
by case/repeat, and calls are sequential. No explicit provider-cache purge is
performed; reported cache evidence must be retained in cost analysis.

Current CLI bootstrap still retrieves only six lessons. Larger batches are a
component-capacity experiment, not a change to the supported retrieval default.
Six measurements per arm/size are exploratory; nearest-rank P95 is the sample
maximum and not an SLA estimate.

## Analyze and price explicitly

```sh
python3 expriment/jev-memory/scripts/analyze_memory_article.py target/memory-article-new-run \
  --price-card expriment/jev-memory/fixtures/memory-article-prices-20260920.json \
  --output target/memory-article-new-run/summary-official.json
```

The article uses the dated official USD price card, not local YAML prices.
On Sunday 2026-09-20 the official Flash off-peak prices were USD 0.15/M fresh
input, USD 0.003/M cache hits, and USD 0.60/M output. Peak rates were twice those
values. [DeepSeek official pricing](https://api-docs.deepseek.com/quick_start/pricing/)
also says the legacy `deepseek-v4-flash` alias is now served by V4.1-Flash.
The response still reports the requested old name in these runs; it is not a
verified weight-version pin. The local YAML's currency/output price comments
are stale and are not used for the article. No local/registered config is changed.

JEV's official input price was USD 0.042/M with free output when checked on
2026-09-20: [TypeSafe model pricing](https://docs.typesafe.ai/models).
The analyzer applies the official cache price to reported cache-read tokens.
Unknown usage remains unknown, including possibly
billed failed calls. Costs with incomplete coverage are lower bounds on the
modeled cost, not exact invoices. Preserve cache-token counts separately so an
actual channel-specific cache price can be applied later.

Report exact selection, precision/recall, false-positive counts, downstream
field-check pass rate, auxiliary and total P50/P95, failure/fallback counts,
reported input/output/cache tokens, auxiliary cost, and total modeled cost.
Do not merge the four truncation cases into the headline core score without
also showing their separate results. The selector sees truncated evidence;
the lexical fallback sees full text. This is a real implementation asymmetry,
not proof that a provider cannot understand the omitted information.
