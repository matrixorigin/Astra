# DeepSeek Flash subagent selection harness

These cases use the existing `astra-test` CLI harness and a live Server and
DeepSeek Flash route. They never contain credentials or a real Offering ID.

Run against a clean candidate build whose CLI and Server report the same Git
revision. Configure the Server, profile and model credentials through the
normal Astra setup. From the repository root, use an empty execution workspace
to reduce accidental exposure to its own harness cases. This changes the
working directory; it is not filesystem isolation or an access-control boundary:

```sh
repo_dir="$(pwd -P)"
export CARGO_TARGET_DIR="$repo_dir/target"
cargo build -p astra-runtime --bin astra-server \
  -p astra-cli --bin astra -p astra-test-harness --bin astra-test
```

Start or restart the configured Server from this build, then verify that its
health revision matches the candidate commit. The harness preflight also
rejects a stale Server. To run and keep the evidence locally:

```sh
repo_dir="$(pwd -P)"
run_dir="$(mktemp -d "${TMPDIR:-/tmp}/astra-subagent-selection.XXXXXX")"
mkdir -p "$run_dir/workspace"
export ASTRA_EXPECTED_BUILD_GIT_SHA="$(git rev-parse HEAD)"
"$repo_dir/target/debug/astra-test" \
  --suite "$repo_dir/crates/astra-test-harness/cases/subagent_model_selection" \
  --astra-bin "$repo_dir/target/debug/astra" \
  --working-dir "$run_dir/workspace" \
  --models deepseek-v4-flash --no-judger --parallel 1 --runs 3 \
  --artifacts-dir "$run_dir/cases" --report-file "$run_dir/report.json" \
  --eval-file "$run_dir/eval.json"
```

Follow the harness preflight instructions in
`crates/astra-test-harness/README.md`. The valid case asserts exactly two
spawn events, one per fanout slot, and checks that both expose the prepared
`deepseek-v4-flash` selection identity in the journal. It also checks that the
prepared child run IDs are distinct and match the IDs returned by that fanout
call. `flash_spawn_configured_name_glm` exercises a cross-model child selected
by exact configured name (`glm-5.2`). It checks the resolved identity, the
child's completed `LlmRound` linked by run ID, and the result in the parent's
final answer. A `launched` receipt alone does not prove the child ran.
Run it only where that name identifies one authorized active Chat model; if
the account has duplicate names, qualify the source in the case for that
deployment. The invalid final-slot case must show zero `agent_spawned` events,
not just a failed terminal answer.

`flash_spawn_natural_language_glm` is a scripted intent-binding check: it
explicitly asks the parent to leave `requested_model_policy` unset (omitted or
`null`) while stating a hard `glm-5.2` requirement. It is not a natural user
journey; `flash_semantic_model_reference_glm` and
`flash_natural_parallel_models_high` cover that boundary.
The child must still be admitted and make its provider request using GLM. This
distinguishes server-side extraction of authenticated user intent from merely
echoing a model selector supplied by the parent model. The server's offline
regression tests additionally verify that one user-intent source is assessed
once and a failed assessment is not automatically retried for the same intent.
The spawn receipt exposes `prepared_model` so the parent can identify the
runtime-selected model without another lookup. That field is pre-execution
evidence, not proof of a provider call; the child `LlmRoundCompleted` event
linked by run ID remains the execution check. A subsequent `agent.get_result`
is normal retrieval and must not count as a second spawn.

`flash_natural_user_news_glm` checks the unscripted user journey: the prompt is
only the original Chinese request, including the ordinary spelling `glm5.2`.
It requires an actual GLM child model round, a child web fetch, and a Sina link
in the final answer. This live-news case depends on the public site; report
site/network unavailability separately from model-selection failures.

`flash_semantic_model_reference_glm` isolates candidate-aware semantic
selection from live websites. The user says `5.2glm` without tool syntax;
the case requires the authorized `glm-5.2` child to make a real provider call
and return a small marker. It forbids Bash, grep and file reads because model
availability comes from the authenticated catalog, never a workspace config
probe. This is one test of a general selector, not a
license to add an alias for that string. Run alongside ambiguous, unavailable,
and source-qualified controls before claiming semantic-selection quality.

`flash_discover_then_delegate_glm` exercises the on-demand authorized Chat
catalog as a user-facing journey. It requires a real `model_catalog` tool
result with no catalog error or failed discovery calls, a GLM child provider round, and the child's
reported marker. The prompt is plain English; it never supplies an Offering
ID or tool syntax. Run it only where GLM 5.2 is authorized. The no-file-probe
checks are an additional UX guard, not proof that every possible probe is
covered.

`flash_spawn_prohibited_model_fail_closed` is an unhappy-path intent check. A
user prohibition such as “do not use `glm-5.2`” is not a positive model choice;
the runtime must keep it unresolved and block the child rather than silently
inheriting DeepSeek Flash or substituting another model.

`flash_missing_child_model_fail_closed` uses an unavailable fixed identity and
checks that no child starts. A parent may decline the delegation itself or
submit a call that admission rejects; neither path permits a silent fallback.
`flash_near_version_must_not_substitute` checks the harder adjacent-version
case: where the authorized catalog has GLM 5.2 but no GLM 5.3, a request for
5.3 must not spawn a 5.2 child or inspect local configuration files. Skip this
case if GLM 5.3 is actually offered.

`flash_fanout_plan_and_high_review` is the fixed-model control journey: one
slot uses GLM-5.2 for a plan-shaped task and another uses DeepSeek Flash with
adaptive high reasoning for a review-shaped task. This pairing follows the
configured wire capabilities: the GLM route exposes a thinking toggle, while
the DeepSeek route exposes an effort-capable thinking protocol. Its
provider-round evidence is linked to the exact child slot and spawn
configuration, so it catches model or reasoning cross-wiring. It is
deliberately a protocol smoke test; it does not claim that the reviewer
consumed the first child's result or that either model produced a high-quality
plan/review.

`flash_natural_parallel_models_high` is the user-facing counterpart. Its
Chinese prompt asks for two parallel helpers, names GLM 5.2 and
deepseek-v4-flash, and asks the latter to think with high effort. It does not
mention Astra tools, selectors, slots, IDs, or schemas. The agent may choose
how to delegate; the oracle checks both child model rounds, the high-effort
prepared configuration, child termination, and mentions of both models in the
parent’s answer. It does not yet verify that the answer accurately attributes
each child finding. A passing run is an end-to-end sample, not a reliability claim; run
several independent trials before quoting a success rate.
The user also permits independent parent analysis, so this case does not ban
repository reads or reflection. Inspect their relevance, round count, and cost
separately; a tool-count ban would conflate useful parallel work with polling.

`flash_reasoning_phrase_is_subject` and `flash_reasoning_correction` test the
single semantic judgment rather than a second keyword parser. The former
asks a child to explain a quoted phrase about high reasoning and verifies that
no high-effort child is started; the latter changes high to medium before
execution and verifies the actual child configuration. Both require a real
child model round and zero Bash, grep or file-read probes. These are samples of a
probabilistic interpretation, not proof that every paraphrase is understood;
run independent variants before claiming reliability. The former's external
oracle cannot by itself distinguish a correct judge result from a parent that
later independently chooses the same effective child settings; inspect the
typed judgment evidence in the session trace before attributing the result.

`flash_fanout_auto_balanced` is a negative control, not evidence that the
router works: it asks for Auto Balanced and verifies that the current product
explains why it cannot route, without silently inheriting the parent model or
starting a child. Auto is intentionally unavailable until comparable
task-level total-cost, quality/reliability, and completion-time evidence exists.
The offline admission test also verifies that a structured Auto tool request
cannot bypass that boundary. This case makes no claim about router quality or
savings.

These cases deliberately do not call `reflect` or add model-request-ledger
reads. The configured-name case proves provider-call identity from the existing
typed step-event capture linked to the spawned child run. Save the report and
structured journal for cost analysis, and report missing usage as unknown
rather than inferring cost from a passing answer.

The report and case artifacts may contain prompts and model responses. Keep the
directory local, inspect it before sharing, and do not commit it. These output
flags write local files; they do not add database reads or writes.
