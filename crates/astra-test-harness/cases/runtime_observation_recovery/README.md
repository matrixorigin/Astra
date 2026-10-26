# Ordinary runtime observation recovery

`ordinary_previous_run` disables Explain capture explicitly. The harness's
default `--explain=on` otherwise hides failures specific to normal conversations.
Its oracle checks the actual prior-run selector, persisted projection, and tool
outcome, not a successful final answer or a model's telemetry claims.

Run this case against each configured primary model with
`astra-test --force-model <model>`. Pair it with the explicit-model delegation
cases when their declared models are available. Production code does not branch
on the primary or delegated model name. Retain failures,
including repeated rejected arguments, and inspect actual child inference and
both delivered values. Provider availability is not a test pass.
