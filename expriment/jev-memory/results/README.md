# Published memory-evaluation results

Start with the final summaries below. This is a curated evidence package, not a
dump of every development run. All retained data comes from synthetic component
scenarios; no real user memories or provider credentials belong here.

## Final measurements — 2026-09-20

The final contribution-based policy was evaluated with three arms and three
repetitions per case: No JEV, native JEV, and Flash JEV-like. Every arm uses the
same downstream Flash model. These four suites contain 432 graded rows,
288 auxiliary judgments, and 432 downstream answers (720 logical model calls).

| Suite | Summary | Complete structured evidence |
| --- | --- | --- |
| 20 core cases and 4 truncation-pressure cases | [main-contribution.json](2026-09-20/main-contribution.json) | [main-contribution-evidence.json](2026-09-20/main-contribution-evidence.json) |
| 2 task families at 6, 24, 96, and 256 candidates | [scale-contribution.json](2026-09-20/scale-contribution.json) | [scale-contribution-evidence.json](2026-09-20/scale-contribution-evidence.json) |
| 8 additional regression cases | [regression-contribution.json](2026-09-20/regression-contribution.json) | [regression-contribution-evidence.json](2026-09-20/regression-contribution-evidence.json) |
| 8 additional generalization checks | [recall-contribution.json](2026-09-20/recall-contribution.json) | [recall-contribution-evidence.json](2026-09-20/recall-contribution-evidence.json) |

Each evidence file retains every case and repetition, including mistakes. It
contains actual judgment inputs, model responses, selected candidates, native
probabilities where available, reported usage, and downstream answers. The
exporter's field allowlist is not a general-purpose privacy scrubber.
Summary files retain aggregate metrics and provenance only; per-case selections
and answers remain in the corresponding evidence file, which is the canonical
source for case-level inspection.

Requests are deduplicated by SHA-256. Each `requests[hash]` entry stores only the
exact serialized `raw` string; obtain its structured view with
`json.loads(entry["raw"])`. Hash the UTF-8 bytes of that string, not a
reserialized object. This preserves the original evidence without storing a
second, derived copy of each request.

## Before/after optimization

These three baselines support the article's contribution-policy comparison and
retrospective threshold analysis. They are separate runs, not extra observations
to pool into the final score.

| Baseline | Summary | Complete structured evidence | Compare against |
| --- | --- | --- | --- |
| Core and truncation | [main-before-contribution.json](2026-09-20/main-before-contribution.json) | [main-before-contribution-evidence.json](2026-09-20/main-before-contribution-evidence.json) | `main-contribution.json` |
| Candidate scale | [scale-before-contribution.json](2026-09-20/scale-before-contribution.json) | [scale-before-contribution-evidence.json](2026-09-20/scale-before-contribution-evidence.json) | `scale-contribution.json` |
| Additional generalization checks | [recall-before.json](2026-09-20/recall-before.json) | [recall-before-evidence.json](2026-09-20/recall-before-evidence.json) | `recall-contribution.json` |

Baseline filenames explicitly distinguish the previous policy from the final
contribution-policy results. Embedded historical run names, measured data, and
provenance are unchanged; a file rename does not change the measured revision.

From the repository root, compare the core results without paid calls:

```sh
python3 expriment/jev-memory/scripts/compare_results.py \
  expriment/jev-memory/results/2026-09-20/main-before-contribution.json \
  expriment/jev-memory/results/2026-09-20/main-contribution.json --group core
```

`compare_results.py` checks fixture hashes, repetitions, deadlines, and prices.
`threshold_sweep.py` only recalculates selection from recorded probabilities;
it does not rerun downstream answers or establish a threshold for new workloads.

## Interpretation and provenance

- These are developer-authored synthetic tests, not a blind benchmark or a
  full agent/tool-loop evaluation. Three repetitions measure stability, not
  three times as many independent tasks.
- Keep truncation-pressure failures separate from the core scores and visible
  alongside them. A selector cannot use evidence omitted by its input budget.
- Cache state was not reset. Reported Flash cache hits substantially affect
  costs, especially in repeated scale cases. All-cache-miss estimates are
  counterfactual calculations, not fresh cold-cache API runs.
- Costs use the [dated official USD price card](../fixtures/memory-article-prices-20260920.json),
  not a current-price guarantee or an invoice. Missing usage is not zero cost.
- Summaries and evidence retain original commit, source/binary hashes, fixture
  hashes, and raw-results hashes. Recorded runs predate delivery-branch rebases;
  a commit field alone does not pin the measured working tree. Do not rewrite
  historical provenance to the PR or merge commit.
- The scale suite has two task families. Its six observations per arm/size make
  nearest-rank P95 the sample maximum, not a production SLA.

## Retention and reproduction

The published package retains seven summaries and seven all-case evidence
files: four final suites and three optimization baselines. Unreferenced
intermediate runs and diagnostic summaries are excluded from the current tree;
the author retains local archival copies. No observations were removed from the
retained runs. Evidence packaging omits only the redundant parsed request view;
raw request strings, responses, metrics, and provenance remain unchanged.

New runs go to ignored, private `target/expriment/jev-memory-*` directories and
never overwrite this snapshot. Review custom inputs and outputs before sharing;
do not commit populated model YAML files, keys, raw private logs, or customer
data. Large future evidence packages can be distributed as versioned release
attachments with checksums instead of growing this directory indefinitely.

Use the [experiment entrypoint](../SKILL.md) to reproduce the protocol and the
[measurement protocol](../references/protocol.md) to interpret its scope and
limits. Remote model aliases, cache state, and service load can change measured
outputs, cost, and latency.
