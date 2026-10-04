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
run_dir="$(mktemp -d "$repo_dir/target/astra-subagent-selection.XXXXXX")"
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

Run the same cases in both selector environments. For a Jev/TypeSafe
deployment, set the exact authorized judgment Offering before the run (the
value below is a deployment input, not a credential):

```sh
astra admin config set judgment_model "$JUDGMENT_OFFERING_ID"
```

For a deployment without Jev, remove only the optional judgment binding and
run the same command:

```sh
astra admin config unset judgment_model
```

The no-Jev trace should show `judgment admission=not_dispatched` and exactly
one `delegation_candidate_assessment` on the primary model on the healthy path.
It must produce the same authorized child bindings. A typed provider deadline
may receive one runtime-owned retry when a full auxiliary slice remains after
the parent convergence reserve. A configured-route fallback consumes that same
second-call allowance; there is never a third logical judgment call. Physical
provider attempts remain separately attributable. Restore the
optional binding after the run when the shared database is used by other
tests. General memory/request judgment remains unavailable in this mode; only
the bounded delegated-model selector uses the primary model as its Jev-like
implementation.

Follow the harness preflight instructions in
`crates/astra-test-harness/README.md`. The valid case asserts exactly two
spawn events, one per fanout slot, and checks that both expose the prepared
`deepseek-v4-flash` selection identity in the journal. It also checks that the
prepared child run IDs are distinct and match the `/agents` launch list returned
by `agent_fanout.start`. This does not require a later `get_results` call when
canonical adoption delivers the results. The live cases intentionally do not
require a parent model to echo an
internal `requested_model_policy` selector. When a user names a model, the
canonical contract is to omit that field and let one candidate-aware admission
bind the request. Structured selector shapes are covered by the offline tool
and admission tests; requiring them in a probabilistic live prompt would test
model compliance with an implementation detail and conflict with the canonical
system instruction. The invalid final-slot case must show zero
`agent_spawned` events, not just a failed terminal answer.

`flash_spawn_natural_language_glm` is a scripted intent-binding check: it
explicitly asks the parent to leave `requested_model_policy` unset (omitted or
`null`) while stating a hard `glm-5.2` requirement. It is not a natural user
journey; `flash_semantic_model_reference_glm` and
`flash_natural_parallel_models_high` cover that boundary.
The child must still be admitted and make its provider request using GLM. This
distinguishes server-side extraction of authenticated user intent from merely
echoing a model selector supplied by the parent model. The server's offline
regression tests additionally verify that one user-intent source is assessed
once; after bounded provider recovery is exhausted, its failed assessment is
cached and is not reissued by later tool rounds for the same intent. Cancellation,
lease loss, or insufficient remaining budget prevents the extra judgment call.
The spawn receipt exposes `prepared_model` so the parent can identify the
runtime-selected model without another lookup. That field is pre-execution
evidence, not proof of a provider call; the child `LlmRoundCompleted` event
linked by run ID remains the execution check. A subsequent `agent.get_result`
is normal retrieval and must not count as a second spawn.

`direct_simple_no_spawn` and `flash_delegate_simple_glm` are the primary
paired efficiency control. Both ask for the same trivial arithmetic answer;
the delegated request naturally names GLM 5.2. Run the direct case once with
DeepSeek Flash and once with GLM 5.2, and the delegated case with DeepSeek
Flash. This isolates selection/spawn/wait/handoff much better than a website
task. The oracle requires exactly one spawn, a real GLM child round, linked
child completion and parent adoption; a parent-computed `42` alone cannot
pass. Both final answers must be exactly `42`. Report all physical calls,
auxiliary judgments, token/cache coverage, tool attempts, and elapsed phases,
not just whether the answer is right. Use repeated runs for latency claims.

`flash_semantic_model_reference_glm` isolates candidate-aware semantic
selection from live websites. The user says `5.2glm` without tool syntax;
the case requires the authorized `glm-5.2` child to make a real provider call
and deliver the exact completed marker through parent adoption or a successful
parent-owned `get_result`. Parent marker text alone cannot pass. It forbids
Bash, grep and file reads because model
availability comes from the authenticated catalog, never a workspace config
probe. This is one test of a general selector, not a
license to add an alias for that string. Run alongside ambiguous, unavailable,
and source-qualified controls before claiming semantic-selection quality.

`flash_discover_then_delegate_glm` exercises the on-demand authorized Chat
catalog as a user-facing journey. It requires a real `model_catalog` tool
result with no catalog error or failed discovery calls, a GLM child provider
round, and the exact completed child result through adoption or parent-owned
retrieval. The current oracle does not prove that catalog discovery preceded
launch or that the observed page contained the selected offering: existing
tool-name ordering checks cannot bind that specific spawn, and fixed array
indices would overfit catalog pagination. Those remain coverage gaps, not
claims made by a passing case. The prompt is plain English; it never supplies an Offering
ID or tool syntax. Run it only where GLM 5.2 is authorized. The no-file-probe
checks are an additional UX guard, not proof that every possible probe is
covered.

`flash_spawn_prohibited_model_fail_closed` is an unhappy-path intent check. A
user prohibition such as “do not use `glm-5.2`” is not a positive model choice;
the runtime must keep it unresolved and block the child rather than silently
inheriting DeepSeek Flash or substituting another model.

`flash_versioned_model_with_independent_parent_task` requires a catalog with a
unique Qwen 3.7 identity (`qwen3.7-max`). It checks the actual child model,
result adoption, and the independent parent's answer. Run it separately from
GLM-only deployments. `flash_rejected_delegation_preserves_parent_tools` checks
that an unavailable child model is still rejected, followed by a successful
parent catalog query rather than a forced text-only turn.

`flash_missing_child_model_fail_closed` uses an unavailable fixed identity and
requires one attempted spawn to receive the typed admission rejection. It does
not accept an omitted child or a parent-only explanation as evidence. The
separate catalog journey covers explicit availability discovery.
`flash_near_version_must_not_substitute` checks the harder adjacent-version
case: where the authorized catalog has GLM 5.2 but no GLM 5.3, a direct request
for 5.3 must be rejected before execution and must not spawn a 5.2 child or
inspect local configuration files. Skip this case if GLM 5.3 is actually
offered.

`flash_fanout_two_models_high` is the fixed-model control journey: one slot
uses GLM-5.2 and another uses DeepSeek Flash with adaptive high reasoning.
Both children return fixed markers. Provider-round evidence is linked to the
exact child slot and spawn configuration, so this test catches model or
reasoning cross-wiring without judging open-ended work. Both adaptive mode and
high effort must belong to Flash's slot 1, not merely exist on some child.

`flash_natural_parallel_models_high` is the user-facing counterpart. Its
Chinese prompt asks for two parallel helpers, names GLM 5.2 and
deepseek-v4-flash, and asks the latter to think with high effort. It does not
mention Astra tools, selectors, slots, IDs, or schemas. The agent may choose
how to delegate; the oracle checks both child model rounds, the high-effort
prepared configuration, child termination, adoption of both fixed markers,
and both markers in the parent's answer. Each marker is unique to its child
task; model execution is verified independently through run-linked provider
events. A passing run is an end-to-end sample, not a reliability claim; run
several independent trials before quoting a success rate. The task requires
no files, network access, or diagnostic tools. Inspect unnecessary calls,
round count, cache usage, and total cost in the trace.
The adoption criterion's `spawn_match` links each result to its required
model (or its fixed fanout slot), so swapping the children's tasks cannot pass.

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

`flash_child_question_parent_answer` checks exactly one spawned child, one
question and answer whose request/message IDs match, completion of that same
child run, and the marker in the parent's final answer. It accepts either an
successful, complete `get_result` by the owning parent, with matching returned
child agent/run identities and exact body, or the
runtime's correlated adoption evidence: the same spawned and completed child
has the expected result hash in a `results_adopted` trace, followed by its
parent’s `finalization_accepted` trace. A queued answer and terminal child
event alone are insufficient. Ordinary `send_message` text (including
`message_type=result`) is not terminal-result evidence. The existing
`session_child_result_adopted` criterion's opt-in `allow_get_result` reuses its
spawn/model/completion identity relation for foreground retrieval; its default
still requires canonical adoption and later accepted finalization.
Even with a matching final answer, this proves
delivery and observation, not that the parent's wording was causally derived
from the child: the marker also appears in the user prompt. It is not a
process-restart test. The initial child task must include both possible marker
outputs but not the chosen format; the later parent answer supplies only the
choice. The journal checks that both outputs reached the spawned child brief;
this keeps the information needed to produce the exact result available without
letting the child skip the question. The user request is explicitly read-only,
and the spawn event must record that workspace scope; this exercises the
delegated tool-discovery and communication surface under read-only authority.
The question/answer value-flow check proves exact request-ID correlation but
does not independently bind both message callers to that child/parent or prove
the queued answer was applied. Those communication-attribution checks remain
outside the current oracle; the final child result is checked independently.

These cases deliberately do not call `reflect` or add model-request-ledger
reads. The configured-name case proves provider-call identity from the existing
typed step-event capture linked to the spawned child run. Save the report and
structured journal for cost analysis, and report missing usage as unknown
rather than inferring cost from a passing answer.

Terminal `4/4`, complete journal attribution, and root prompt-cache evidence
are not full child-expense coverage or task-quality proof. `inclusive_tokens`
counts observed usage only; missing child usage remains unknown, not zero.
Root-cache ratios describe the root execution, not a whole-task child-inclusive
cache denominator. Report these limits even when every structural case passes.

The report and case artifacts may contain prompts and model responses. Keep the
directory local, inspect it before sharing, and do not commit it. These output
flags write local files; they do not add database reads or writes.
