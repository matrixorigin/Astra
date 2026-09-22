# DeepSeek Flash subagent selection harness

These cases use the existing `astra-test` CLI harness and a live Server and
DeepSeek Flash route. They never contain credentials or a real Offering ID.

Run against a clean candidate build whose CLI and Server report the same Git
revision. Configure the Server, profile and model credentials through the
normal Astra setup, then run:

```sh
astra-test --suite crates/astra-test-harness/cases/subagent_model_selection \
  --models deepseek-v4-flash --no-judger --parallel 1 --runs 3
```

For revision-bound evidence set `ASTRA_EXPECTED_BUILD_GIT_SHA` to the full
candidate commit SHA and follow the harness preflight instructions in
`crates/astra-test-harness/README.md`. Save the report and structured journal,
including actual child model/Offering and provider usage; a passing prompt
alone is not proof of model identity or cost. The invalid final-slot case must
show zero `agent_spawned` events, not just a failed terminal answer.
