# Bounded review journey

These opt-in cases create a three-commit repository with two small regressions,
copy the actual project review skill, and evaluate ordinary versus explicitly
independent review. They deliberately do not run against Astra's large diff.
`setup.py` refuses a nonempty working directory. Keep the resulting reports and
session captures; neither assistant prose nor an exit code alone proves success.

Use a candidate CLI, harness and Server from the same clean revision, configured
authentication, and an available model. Run each arm in a separate empty directory
with `--parallel 1 --runs 1`. From the source root, set
`ASTRA_REVIEW_JOURNEY_SOURCE="$PWD/crates/astra-test-harness/fixtures/review_journey"`
and export it for the setup hook. For each arm:

```sh
mkdir -p target/test-tmp
fixture=$(mktemp -d "$PWD/target/test-tmp/review-journey.XXXXXX")
astra-test --suite "$ASTRA_REVIEW_JOURNEY_SOURCE" \
  --filter ordinary_review_owns_current_execution \
  --working-dir "$fixture" --astra-bin /absolute/path/to/candidate/astra \
  --models AVAILABLE_MODEL --judger-model AVAILABLE_JUDGE --parallel 1 --runs 1
```

Repeat with a fresh directory and filter `explicitly_required_review_inherits_model`.
The primary and judge are caller-selected; no fixture model is hardcoded. Check
the captured model requests and child lineage for actual model inheritance and
bounded evidence handoff. The ordinary arm requires actual skill loading, no
delegation and no explicit Bash verification. Both arms must identify both real
regressions; the independent arm also requires terminal child-result adoption.
The YAML score alone does not certify inheritance: reconcile the resolved child
Offering and reasoning with the root and verify successful skill loading in the
child that owns the independent review, using the captured run identities.
Record rounds, tools, usage/cache coverage and time separately. These two small
journeys are regression diagnostics, not a general performance benchmark.
