# Skill Selector Benchmark Suite

Offline accuracy benchmark for the runtime skill selector. Use this when you
change the selector (lexical scoring, embedding fusion, rerank, …) and want
hit@k numbers on a fixed 1000-skill / 1000-prompt corpus before opening a PR.

## Layout

```
benchmarks/skill-selector/
├── runner/        # Rust bin: runs the real Astra selector over the corpus
├── validator/     # Rust bin: sanity-checks SKILL.md / metadata of the corpus
├── scripts/       # Python: corpus curation, dataset generation, prototypes
├── dataset/       # primary.jsonl + catalog.jsonl + manifests (committed)
├── results/       # local benchmark outputs, NOT committed (see .gitignore)
├── skills.tar.gz  # 1000-skill corpus body (committed, 21 MB)
└── skills/        # extracted corpus, NOT committed (see .gitignore)
```

## Setup (once per checkout)

Extract the corpus:

```bash
mkdir -p benchmarks/skill-selector/skills
tar -xzf benchmarks/skill-selector/skills.tar.gz \
  -C benchmarks/skill-selector/skills
```

That gives you 1000 skill directories under `skills/`. Re-run this if you
update `skills.tar.gz`.

## Run the selector benchmark

From the repo root:

```bash
cd benchmarks/skill-selector/runner
cargo run --release -- \
  --sample-dir ../skills \
  --dataset    ../dataset/primary.jsonl \
  --output     ../results/primary-selector-run.jsonl \
  --summary    ../results/primary-selector-run-summary.json
```

Useful flags:

- `--limit N` smoke-test against the first N records.
- `--include-unpassed` count records that did not pass the LLM judge filter.

The runner writes a per-prompt JSONL row plus an aggregate summary JSON
(hit@1 / hit@3 / hit@5 / hit@14 etc.). The `results/` directory is ignored so
large benchmark outputs do not get committed accidentally.

If `MEMORIA_EMBEDDING_BASE_URL` + `MEMORIA_EMBEDDING_API_KEY` are set, the
selector's embedding-fusion path is exercised; otherwise it runs lexical-only.

## Validate the corpus

```bash
cd benchmarks/skill-selector/validator
cargo run --release -- ../skills
```

Reports any SKILL.md files that fail the loader contract.

## Regenerate the corpus / dataset (advanced)

The Python scripts under `scripts/` rebuild everything from upstream skill
libraries. They are kept in-tree for reproducibility but require external
inputs (`claude-skills`, `antigravity-awesome-skills`, …) that are NOT in
this repo. Read the script headers before running.

- `curate_skill_corpus.py`         — curate astra-compatible skills + quarantine.
- `generate_selector_benchmark.py` — generate primary / hard / no-skill prompts.
- `prototype_*.py`                 — algorithmic prototypes (hybrid / rerank).

## Reference baseline notes

Quick reference from the initial experiments against this corpus:

| Variant                                | hit@1 | hit@3 | hit@5 | hit@14 |
| -------------------------------------- | ----- | ----- | ----- | ------ |
| current selector (metadata, lexical)   | 3.1%  | 5.4%  | 6.2%  | 10.2%  |
| pure embedding (BAAI/bge-m3, top-k)    | 38.1% | 53.7% | 60.5% | 71.8%  |
| chunked-fulltext embedding (top1/10/30/50) | 36.9% / 69.4% / 81.9% / 85.9% |

Re-run the benchmark locally to produce fresh `results/*-summary.json` details.
