# Compare two Skill revisions

Evaluation compares two fixed revisions against the same frozen task and
expected result. The first supported criterion checks whether the entire
assistant output is the expected JSON value. It does not measure general task
quality from prose.

Use an authenticated Server API connection. Both Skill revisions must belong
to your account and support instruction-only, inline execution. Skillify can
produce a candidate revision; Evaluation owns the comparison and its results.

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
    "verifier_config": {"expected": {"ok": true}}
  },
  "model_offering_id": "OFFERING_ID",
  "max_concurrency": 1,
  "max_wall_time_secs": 120
}
```

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
| Assessment pass | The complete recorded output matched the frozen JSON criterion. |
| Assessment fail | The complete recorded output did not match that criterion. |
| Assessment unavailable | A proven limitation prevents a criterion verdict; no success value is assigned. |
| Assessment pending | Evidence is not ready; retry assessment without rerunning the trial. |
| Metric gap | That dimension lacks sufficient measurement evidence. |

A passed criterion closes only task coverage. Token subtotals are not complete
cost, and missing provider, context, tool, safety, or reliability evidence stays
visible. Use the report's evidence references when reviewing a comparison;
neither a completed Run nor a confident final answer establishes overall success.

See the [Evaluation contract](../design/evaluation.md) for persistence,
isolation, and evidence ownership.
