# CI and GitHub Actions

## Overview

CI runs on GitHub Actions. Each concern has its own workflow under `.github/workflows/`. Workflows run on triggers (push, pull_request, etc.) and can be extended over time.

## Layout

```
.github/
  workflows/           # one workflow file per concern
    pr-title.yml       # PR title validation (first action)
    (future)           # e.g. test.yml, lint.yml, release.yml
  (optional)           # actions/, CODEOWNERS, etc.
```

## Adding a new action

1. Add a new `.yml` under `.github/workflows/`.
2. Set `name`, `on`, and `jobs`; use `workflow_dispatch` if you want manual runs.
3. Document the workflow in this file (or a short comment in the YAML).

No single “monolith” workflow: keep workflows independent so they are easy to add, disable, or change.

## Current workflows

### 1. PR title validation (`pr-title.yml`)

- **Trigger**: `pull_request` (opened, edited, synchronize) so every update to the PR (including title change) is checked.
- **Purpose**: Block PRs whose title does not match the required convention.
- **Convention**: [Conventional Commits](https://www.conventionalcommits.org/) style:
  - `type(scope?): description`
  - Allowed types: `feat`, `fix`, `docs`, `style`, `refactor`, `perf`, `test`, `chore`, `build`, `ci`, `revert`.
  - Optional scope in parentheses; description required after colon and space.
- **Behavior**: Job fails if the title does not match; status appears on the PR and can be required as a branch protection check.

To change the pattern, edit the `env` or the script in `pr-title.yml`.

**To block merge on failure**: In the repo → Settings → Branches → Branch protection rule for `main` (or your default) → Require status checks to pass → add **"Check PR title"** (the job name). Then PRs with invalid titles cannot be merged until the title is fixed.
