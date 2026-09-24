# Compare two Skill revisions

Evaluation compares two fixed revisions against the same frozen task and
expected result. The first supported criterion checks whether the entire
assistant output is the expected JSON value. It does not measure general task
quality from prose.

Use an authenticated Server API connection. Both Skill revisions must belong
to your account and support instruction-only, inline execution. Skillify can
produce a candidate revision; Evaluation owns the comparison and its results.

## Use the CLI or Web flow

The CLI and Web surface are thin clients of the same control plane. They submit
the prepare intent, start the planned trials through the canonical Run
backbone, follow the owner-scoped projection, request durable assessment, and
render the returned report. They do not keep a second experiment lifecycle.

For the CLI, save the prepare JSON as `evaluation-intent.json` and run:

```sh
astra evaluation run evaluation-intent.json
astra evaluation show <experiment-id>
astra evaluation report <experiment-id>
```

The Web flow is available at `/evaluations`. It lists the authenticated user's
published instruction-only Skills, primary model Offerings, and typed-judgment
Offerings. Leaving Judgment Offering at `Use configured default` lets the
server freeze the value configured for the authenticated user, including a Jev
Offering from `.models.yaml`; choosing an Offering freezes that exact value.
The page only displays server projections and report coverage, so an
interrupted browser run remains durable and can be resumed through the CLI
or Web page. The browser keeps only the owner/runtime-scoped experiment
reference and, while `prepare` is in flight, its submission idempotency key;
if the prepare response is lost, the page resolves that key on the server
before offering resume. Prompt and verifier content are not browser recovery
state.

## Prepare the comparison

Send `POST /evaluation/experiments/prepare` with your revision and Offering IDs:

```json
{
  "submission_idempotency_key": "compare-extraction-001",
  "target": {
    "kind": "skill",
    "skill_name": "extract-result",
    "baseline": {"revision_id": "BASELINE_VERSION_ID"},
    "candidate": {"revision_id": "CANDIDATE_VERSION_ID"}
  },
  "case": {
    "case_id": "structured-result",
    "message": "Apply the Skill and return exactly the JSON result.",
    "verifier_config": {"kind": "json_value_equals", "expected": {"ok": true}}
  },
  "model_offering_id": "OFFERING_ID",
  "max_concurrency": 1,
  "max_wall_time_secs": 120
}
```

For a workspace-backed comparison, add an authenticated Edge executor, the
full commit to evaluate, and the exact built-in tools the model may see:

```json
{
  "workspace": {
    "edge_executor_id": "EDGE_AGENT_ID",
    "source_commit": "FULL_40_OR_64_HEX_COMMIT",
    "tool_names": ["bash", "read_file", "write_file"]
  }
}
```

Replace the case verifier with the frozen command that establishes the coding
claim:

```json
{
  "verifier_config": {
    "kind": "workspace_command",
    "command": "make check",
    "expected_exit_code": 0,
    "timeout_secs": 120
  }
}
```

The command runs after agent execution with network disabled against a clean
clone reconstructed from the captured patch. Its output, exit code, final
patch, source identity, and isolation proof are persisted before the workspace
can be released. Symlinks and nested Git repositories are unsupported in this
profile. Missing replay, isolation, settlement, or a mutating verifier is
reported as unavailable.

The server freezes this policy into the experiment. The selected Edge must be
registered for the owner with the same root and materialization identity, and
its authenticated capability advertisement must prove the current source
checkout, a source tree, and every requested tool. Before execution, the Edge
creates an independent clone for this trial start attempt, checks out the frozen commit, and
returns a live source/tree/clean snapshot that must match that frozen commit.
The server-authored clone root is carried through
the durable tool envelope and the workspace materialization is recorded as a
required receipt. A missing or changed proof leaves the trial unavailable; the
server does not fall back to its own filesystem or shell. Clean clones are
released only on the creating connection generation, while dirty clones remain
available as evidence.
The ordinary `astra evaluation run intent.json` command and the Web flow use
this same request shape.

Save the returned experiment ID and trial IDs. The server freezes the Skill
content, model configuration, prompt inputs, execution budgets, and verification
criterion. The expected value is not inserted into the trial prompt. Retrying
the same submission key and intent returns the existing experiment. Use a new
key for a different comparison.

## Execute, assess, and compare

For each returned trial, in sequence order:

1. Send `POST /evaluation/experiments/{experiment_id}/trials/{trial_id}/start`
   with `{}`. Save the returned Run and Session IDs. Retrying this request
   returns the same Run.
2. Read `GET /evaluation/experiments/{experiment_id}` to follow execution.
3. Send `POST /evaluation/experiments/{experiment_id}/trials/{trial_id}/assess`
   without an output or verdict body. HTTP 202 means evidence is not ready;
   retry the same endpoint. HTTP 200 returns the immutable assessment.
4. Start the next trial after the first trial's terminal observation exists.

If execution has finished but its observation is missing, the assess endpoint
repairs that observation from durable Run evidence. It does not rerun the model.
A temporary storage failure returns 503 and can be retried. A 409 indicates an
evidence or identity conflict; it is not a failed task criterion.

Read `GET /evaluation/experiments/{experiment_id}/report` for the structured
comparison, Markdown report, and evidence manifest. Status and report GETs
never trigger execution or verification.

## Interpret the result

| Result | Meaning |
| --- | --- |
| Run completed | Execution ended successfully; the task criterion may still fail. |
| Assessment pass | The frozen JSON criterion matched, or the frozen workspace command completed with its expected exit code and complete evidence. |
| Assessment fail | The applicable frozen criterion was evaluated and did not pass. |
| Assessment unavailable | A proven limitation prevents a criterion verdict; no success value is assigned. |
| Assessment pending | Evidence is not ready; retry assessment without rerunning the trial. |
| Metric gap | That dimension lacks sufficient measurement evidence. |

A passed criterion closes only task coverage. Token subtotals are not complete
cost, and missing provider, context, tool, safety, or reliability evidence stays
visible. Use the report's evidence references when reviewing a comparison;
neither a completed Run nor a confident final answer establishes overall success.

The report's tool metrics come from the durable run-accounting event. Tool
validity is successful requested calls divided by requested calls; a run with
no requested call has no validity ratio. Policy violations count attempted calls
rejected by the explicit policy/safety boundary. Cost comes from every physical
inference attempt and remains unavailable when usage, terminal state, or the
admitted route price is missing. A provider-mismatched attempt is not counted as
priced because the current price snapshot is route-scoped. Provider fallback
count comes from the provider identity on every physical attempt compared with
its admitted invocation route; incomplete route identity leaves it unknown.
The current frozen adapter does not switch providers, so zero proves route
consistency for that run and does not claim that a Jev-to-LLM fallback chain is
enabled.

See the [Evaluation contract](../design/evaluation.md) for persistence,
isolation, and evidence ownership.

## Compare the Skill routing judgment

To measure the value of the auto-route decision point itself, use the same
published Skill revision on both arms:

```json
{
  "submission_idempotency_key": "compare-skill-routing-001",
  "target": {
    "kind": "skill_routing_judgment",
    "skill_name": "review-changes",
    "baseline": {"revision_id": "PINNED_VERSION_ID"},
    "candidate": {"revision_id": "PINNED_VERSION_ID"}
  },
  "case": {
    "case_id": "routing-case",
    "message": "Review the current branch and report the findings.",
    "verifier_config": {"kind": "json_value_equals", "expected": {"ok": true}}
  },
  "model_offering_id": "PRIMARY_OFFERING_ID",
  "judgment_model_offering_id": "JEV_OFFERING_ID",
  "max_concurrency": 1,
  "max_wall_time_secs": 120
}
```

Omit `judgment_model_offering_id` to use the configured judgment Offering,
which is frozen during prepare. If no default judgment Offering is available,
the candidate keeps the basic Skill path and the report cannot establish a Jev
benefit; the fallback run must not be interpreted as a successful Jev
comparison.
