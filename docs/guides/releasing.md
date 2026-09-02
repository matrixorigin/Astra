# Releasing Astra

This guide describes the maintainer workflow for publishing Astra CLI binaries
and container images. Releases use semantic versions and immutable Git tags.

## Release contract

A tag named `vX.Y.Z` triggers both release workflows:

- `release-binaries.yml` publishes CLI archives for Linux and macOS on AMD64
  and ARM64, plus SHA-256 checksum files, to a GitHub Release.
- `release-docker.yml` publishes the multi-architecture
  `matrixorigin/astra:X.Y.Z` image to Docker Hub.

Stable releases also update the `X.Y` and `latest` container tags. A prerelease
such as `v0.2.0-rc.1` is marked as a GitHub prerelease and does not update those
rolling tags.

## Prepare the release

1. Open a release pull request from a feature branch.
2. Update the workspace version in `Cargo.toml` and any user-facing version
   references that apply to the release.
3. Confirm that the version matches the intended tag without the leading `v`.
4. Summarize user-visible changes, compatibility impact, and migration steps in
   the pull request.
5. Apply the most specific `kind/*`, `documentation`, or `improvement` label to
   each included pull request so the generated release notes are categorized.
6. Run `make check`, `make test-offline`, and any integration lane required by
   the affected boundaries.
7. Merge only after required CI and review pass on the exact release commit.

## Publish

Create an annotated tag on the reviewed commit and push that tag:

```bash
git switch main
git pull --ff-only
git tag -a v0.2.0 -m "Astra v0.2.0"
git push origin v0.2.0
```

Do not move or reuse a published version tag. The manual workflow inputs exist
for recovery and targeted builds; a normal release starts from the Git tag so
the source, binaries, and container image identify the same commit.

## Verify the release

Wait for both release workflows to pass, then verify:

- The GitHub Release points to the intended commit and has all four CLI
  archives, their individual checksums, `checksums.txt`, and its checksum.
- Generated release notes group the included pull requests accurately.
- A downloaded archive matches its published SHA-256 value.
- The Docker version tag resolves to both `linux/amd64` and `linux/arm64`.
- The binary reports the expected version and completes a basic CLI health
  check.
- For a stable release, the `X.Y` and `latest` tags resolve to the new manifest;
  for a prerelease, they remain unchanged.

## Correct a bad release

Do not rewrite the Git tag or replace release assets in place. Fix the problem
through the normal pull request workflow and publish a new patch version. If an
artifact is unsafe to use, mark its GitHub Release as a prerelease and state the
replacement version prominently while the fix is prepared.
