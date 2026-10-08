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

Fixed model selection has the same runtime contract with and without Jev.
The primary proposes the selected model using the canonical tool control;
runtime authorizes that exact identity and reasoning capability. There is no
mandatory auxiliary model-selection call, so the trace must not attribute a
removed selector's synthetic usage. Restore optional bindings after tests
against a database shared with other callers.

Follow the harness preflight instructions in
`crates/astra-test-harness/README.md`. The fanout oracle checks prepared model
identity and distinct child run IDs against the launch list. Discovery and
configured-name resolution use the authenticated catalog, never local files.
Cold initialization loads and caches the authorized catalog, including ordinary
turns. The primary receives a bounded page outside the stable system/tool prefix;
execution still checks authorization. Partial/unknown choices may require
discovery; keep the strict zero-discovery
efficiency cases as separate acceptance evidence, rather than weakening them
or presenting a functional pass as an efficiency pass.

Retired scripted cases that required rejection despite leaving model policy
unset tested the old auxiliary interpreter, not the current inheritance contract.
A prohibition of a different model does not itself prohibit the parent's model.
The family, version, content-role and actual-child oracles remain; unavailable
fixed selectors require the canonical typed `invalid_request` receipt and zero
children. A correct parent answer alone never proves correct child execution.
The spawn receipt exposes `prepared_model` so the parent can identify the
runtime-selected model without another lookup. That field is pre-execution
evidence, not proof of a provider call; the canonical capture requires a
physical provider request linked to the exact child run. A subsequent `agent.get_result`
is normal retrieval and must not count as a second spawn.

`flash_model_name_is_output` is a negative intent control: a quoted model name
is the child's requested output, not its executor. It requires an actual
inherited-model child request and adoption of that child's exact result. Run
it alongside natural family-name assignment and unavailable-version cases;
matching an answer string alone is not evidence of correct model binding.

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

`flash_child_compact_json` adds a non-arithmetic, compact JSON contract for
both the actual GLM child result and the parent's answer. Together with the
integer-only cases, it checks whether requested formats survive shared persona
and summary guidance without runtime output rewriting. Keep failed samples;
one later pass does not demonstrate reliable format compliance. The fanout
case also reports a soft primary-round bound of four: automatic delivery should
avoid re-fetching sufficient observed results, while inspection, missing or
truncated output, pagination and recovery remain valid reasons to read results.

`flash_semantic_model_reference_glm` isolates natural-language model
selection from live websites. The user says `5.2glm` without tool syntax;
the case requires the authorized `glm-5.2` child to make a real provider call
and deliver the exact completed marker through parent adoption or a successful
parent-owned `get_result`. Parent marker text alone cannot pass. It forbids
Bash, grep and file reads because model
availability comes from the authenticated catalog, never a workspace config
probe. This is one test of a general selector, not a
license to add an alias for that string. Run alongside ambiguous, unavailable,
and source-qualified controls before claiming semantic-selection quality.

`flash_scoped_child_and_parent` uses a family-only model reference and separately
assigns work to the child and primary agent, without tool syntax or instructions
forbidding discovery. Run it only with exactly one authorized GLM candidate
identity. Its oracle checks actual child inference, adoption, both results,
zero discovery/file/network tools, and at most three primary rounds. Explicit
version cases do not establish family-reference quality; this case must pass
alongside ambiguous and unavailable candidate controls, not replace them.

`flash_discover_then_delegate_glm` exercises authorized Chat availability and
delegation. A complete supplied catalog can avoid another `model_catalog` call.
The oracle retains zero failed discovery calls, a real GLM child provider
round, and exact completed child-result adoption or parent-owned retrieval.
The explicit-query journey is covered separately by
`flash_rejected_delegation_preserves_parent_tools`. This availability oracle
does not prove catalog discovery preceded
launch or that the observed page contained the selected offering: existing
tool-name ordering checks cannot bind that specific spawn, and fixed array
indices would overfit catalog pagination. Those remain coverage gaps, not
claims made by a passing case. The prompt is plain English; it never supplies an Offering
ID or tool syntax. Run it only where GLM 5.2 is authorized. The no-file-probe
checks are an additional UX guard, not proof that every possible probe is
covered.

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
The adoption criterion binds each complete result to its actual model and,
where requested, fanout slot; swapping the children's tasks cannot pass.

`flash_reasoning_phrase_is_subject` and `flash_reasoning_correction` test
interpretation through the primary's typed proposal, without a keyword parser. The former
asks a child to explain a quoted phrase about high reasoning and verifies exact
inheritance of the parent's admitted Offering and thinking configuration;
the latter changes high to medium before
execution and verifies the actual child configuration. Both require a real
child model round and zero Bash, grep or file-read probes. These are samples of a
probabilistic interpretation, not proof that every paraphrase is understood;
run independent variants before claiming reliability. The former's external
oracle establishes the actual child configuration, not the primary's private
reasoning. Inspect proposal, admission and run-linked provider facts before
attributing the result; no auxiliary interpretation is required.

`flash_fanout_auto_balanced` is a negative control, not evidence that the
router works: it asks for Auto Balanced and verifies that the current product
explains why it cannot route, without silently inheriting the parent model or
starting a child. Auto is intentionally unavailable until comparable
task-level total-cost, quality/reliability, and completion-time evidence exists.
The offline admission test also verifies that a structured Auto tool request
cannot bypass that boundary. This case makes no claim about router quality or
savings.

`flash_child_question_parent_answer` checks exactly one spawned child, one
successful child-owned question and root-owned answer with the exact request
ID, and typed durable Sent/Received evidence binding both parties and the
answer message ID. The same child must complete, execute on GLM, and have its
complete raw result digest adopted before parent finalization. A queued answer
alone is insufficient. Ordinary `send_message` text (including
`message_type=result`) is not terminal-result evidence.
Even with a matching final answer, this proves
delivery and observation, not that the parent's wording was causally derived
from the child: the marker also appears in the user prompt. It is not a
process-restart test. The initial child task must include both possible marker
outputs but not the chosen format; the later parent answer supplies only the
choice. Conjoined canonical predicates check that both outputs reached the
same successful initial spawn brief;
this keeps the information needed to produce the exact result available without
letting the child skip the question. The user request is explicitly read-only,
and the spawn fact must record `read_only` mutation intent. This is a recorded
intent check, not independent proof of the complete sandbox permission ceiling.
Completion relies on the existing reply-obligation fence, which clears only
after a successful provider attempt consumes the correlated response; a
Received event alone does not establish model inclusion.

These tasks do not ask the agent to call `reflect`. The harness separately
captures the existing Server execution view, including authenticated run tree,
Reflect facts, complete paged transcript, and optional admitted-control tails.
Those test-only reads are not part of the agent's ordinary execution overhead.
Save the report and canonical capture for cost analysis, and report missing usage as unknown
rather than inferring cost from a passing answer.

Terminal `4/4`, complete journal attribution, and root prompt-cache evidence
are not full child-expense coverage or task-quality proof. `inclusive_tokens`
counts observed usage only; missing child usage remains unknown, not zero.
Root-cache ratios describe the root execution, not a whole-task child-inclusive
cache denominator. Report these limits even when every structural case passes.

The report and case artifacts may contain prompts and model responses. Keep the
directory local, inspect it before sharing, and do not commit it. These output
flags write local files; they do not add database reads or writes.
