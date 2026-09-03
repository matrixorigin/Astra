# Releasing Astra

This guide describes the maintainer workflow for publishing Astra from the
`matrixorigin/Astra` repository. Releases use semantic versions and immutable
Git tags; the GitHub Release, installer, source, and release automation all
live here. Docker images are published as `matrixorigin/astra` on Docker Hub.

## Release contract

A tag named `vX.Y.Z` triggers both release workflows:

- `release-binaries.yml` publishes CLI archives for Linux and macOS on AMD64
  and ARM64, plus SHA-256 checksum files, to a GitHub Release.
- `release-docker.yml` publishes the multi-architecture
  `matrixorigin/astra:X.Y.Z` image to Docker Hub.

The Docker workflow first starts each untagged platform image by its immutable
digest through the documented all-in-one deployment, then verifies API health
plus an exact memory write/retrieval. Only after every platform passes does the
workflow publish the `X.Y.Z` manifest. A stable release then updates `X.Y` and
`latest`; a prerelease such as `v0.2.0-rc.1` is marked as a GitHub prerelease
and does not update those rolling tags. A failed candidate therefore changes no
user-facing Docker tag.

Both workflows reject a release tag whose commit is not reachable from the
repository's default branch. They run independently so a failed platform does
not conceal a successful one, but a release is complete only when both are
green. The binary workflow needs only the repository-provided `GITHUB_TOKEN`;
there is no cross-repository release token or secondary release mirror.

## Repository configuration

The Docker workflow requires these repository secrets:

- `DOCKERHUB_USERNAME`
- `DOCKERHUB_TOKEN`

The optional private registry mirror runs only when both
`CONTAINER_MIRROR_REGISTRY` and `CONTAINER_MIRROR_IMAGE` repository variables
are set. It additionally uses `IDC_REGISTRY_USERNAME` and
`IDC_REGISTRY_PASSWORD`; proxy and runner variables are documented inline in
`release-docker.yml`. These settings copy an already verified Docker manifest
and do not own the public GitHub Release.

The source tree also versions the public `@astra/sdk` package and the Helm
chart, but the tag workflows do not publish either to npm or a chart registry.
Until dedicated publication gates exist, treat those as explicit maintainer
actions and do not advertise them as tag-produced artifacts.

## Prepare the release

1. Open a release pull request from a feature branch.
2. Update the release version in `Cargo.toml`, `packages/sdk/package.json`, its
   package lock, `web/package.json`, its package lock, `CITATION.cff`, and both
   `version` and `appVersion` in `deployment/kubernetes/chart/Chart.yaml`.
   Update `Cargo.lock` when the workspace version changes.
3. Confirm that the version matches the intended tag without the leading `v`.
   Both tag-triggered release workflows reject a mismatched workspace version
   before building or publishing artifacts.
4. Summarize user-visible changes, compatibility impact, and migration steps in
   the pull request.
5. Apply the most specific `kind/*`, `documentation`, or `improvement` label to
   each included pull request so the generated release notes are categorized.
6. Run `scripts/validate-release-version.sh X.Y.Z`, `make check`,
   `make test-offline`, and any integration lane required by
   the affected boundaries.
7. Merge only after required CI and review pass on the exact release commit.

## Publish

Create an annotated tag on the reviewed commit and push that tag:

```bash
git switch main
git pull --ff-only
git tag -a v0.2.0 -m "Astra v0.2.0"
make release-check VERSION=0.2.0
git push origin v0.2.0
```

`make release-check` is deliberately read-only: it verifies version metadata,
the clean worktree, and the tag-to-commit relationship. It never builds or
pushes an image. Do not move or reuse a published version tag. The binary
workflow's manual input is for recovering a failed publication from an existing
tag, not for selecting unreviewed source. Manual Docker runs publish snapshot
tags only; semantic-version Docker releases must start from the matching Git
tag so the source, binaries, and container image identify the same commit.

## Verify the release

Wait for both release workflows to pass, then verify:

- The GitHub Release points to the intended commit and has all four CLI
  archives, their individual checksums, `checksums.txt`, and its checksum.
- Each CLI archive contains the `astra` executable and the Apache-2.0
  `LICENSE` file at its root.
- Generated release notes group the included pull requests accurately.
- A downloaded archive matches its published SHA-256 value.
- The documented installer downloads from `matrixorigin/Astra`, rejects a
  missing checksum, and reports the expected version from `astra --version`.
- The Docker version tag resolves to both `linux/amd64` and `linux/arm64`.
- The Docker runtime smoke passed on every built platform, including the
  all-in-one health and memory round-trip checks.
- The binary reports the expected version and completes a basic CLI health
  check.
- For a stable release, the `X.Y` and `latest` tags resolve to the new manifest;
  for a prerelease, they remain unchanged.

## Correct a bad release

Do not rewrite the Git tag or replace release assets in place. Fix the problem
through the normal pull request workflow and publish a new patch version. If an
artifact is unsafe to use, mark its GitHub Release as a prerelease and state the
replacement version prominently while the fix is prepared.
