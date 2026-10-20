## Summary

<!-- What changed, why is it needed, and who benefits? -->

## Related issue

<!-- Use "Closes #123" when merging this PR should close an issue. -->

## Change type

- [ ] Feature
- [ ] Bug fix
- [ ] Documentation
- [ ] Refactor or performance improvement
- [ ] Test
- [ ] Build, CI, or maintenance

## User and compatibility impact

<!-- Describe visible behavior, configuration or API changes, and migration
requirements. Write "None" when there is no user-visible or compatibility
impact. -->

## Architecture and complexity delta

<!-- Required for runtime, architecture, or state changes; otherwise write
"N/A". Keep one canonical implementation and call out any intentional overlap. -->

- Canonical owner changed or extended:
- Existing implementations and callers searched:
- Superseded code, states, tables, shims, or self-only tests removed:
- If parallel implementations remain, their boundary and retirement condition:

## Capability assessment

<!-- Required by CI for every PR, including dependency and release PRs. Choose
one disposition and provide an explanation:
Updated: name the affected capability, status/profiles and reference update.
Unchanged: identify the existing assessment and why it remains accurate.
N/A: explain why this change has no capability impact.
Review checks the explanation; CI does not infer capability impact from code. -->

## Verification

<!-- List the exact commands or manual checks run and their results. Include the
public product entrypoint and unhappy paths when behavior changes. For database
changes, state how schema, queries, transactions, and migrations were verified
against a real database, or explain why that is not applicable. -->

- Commands and results:
- Public entrypoint exercised:
- Unhappy paths exercised:
- Database verification:

## Final checklist

- [ ] I added or updated tests at the layer that owns the behavior, or
      explained why no test is needed.
- [ ] I updated public or design documentation for contract changes, or the
      change needs no documentation update.
- [ ] I checked the diff for credentials, private URLs, customer data,
      generated files, and other sensitive information.
- [ ] The PR title follows the repository's Conventional Commit format.
