# Examples

The old checked-in Python examples are gone.

Current runnable examples for this repository are:

- the CLI flows in `README.md`
- the contract tests in `rust/crates/api-shell/tests/`
- the local development commands exposed by `make help`

## Recommended Hands-On Paths

```bash
make dev-init
make dev-start
make test-integration
```

Then try:

```bash
mo-agent login
mo-agent chat -m "帮我分析这个仓库"
mo-admin login
mo-admin audit --limit 20
```
