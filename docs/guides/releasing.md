# Releasing Astra

This guide defines the maintainer workflow for publishing Astra from the
`matrixorigin/Astra` repository. One protected workflow owns the source tag,
GitHub Release, client archives, public server image, rolling Docker tags, and
optional registry mirror. A release is selected once, verified as one candidate
set, and only then made visible to users.

## What one release contains

| Runtime role | Published form | User entrypoint |
| --- | --- | --- |
| CLI | Checksum-verified GitHub Release archive | `astra` |
| Edge / User Runner | The same client archive | `astra-edge` |
| Server | Verified multi-platform image | `matrixorigin/astra:X.Y.Z` |
| Local dependencies | Versioned compatibility pins | all-in-one `.env.example` |
| Web dashboard | Source checkout for now | development workflow |

The client set covers Linux and macOS on AMD64 and ARM64. Each build executes
both binaries before packaging them. The server set covers Linux AMD64 and
ARM64; each untagged digest is started through the documented all-in-one stack
and must pass API readiness, dependency health, and an exact memory
write/retrieval/cleanup round trip.

Stable releases update the Docker `X.Y` and `latest` tags only after the
versioned Docker manifest and GitHub Release are both available. A prerelease
such as `0.2.0-rc.1` publishes only its exact version and is marked as a GitHub
prerelease.

## Why publication starts from a workflow, not a tag push

Tag-triggered workflows execute release logic stored with the tagged commit.
Allowing any historical commit reachable from `main` to initiate publication
therefore lets obsolete automation become the release control plane.

The **Release Astra** workflow is manually dispatched from the protected
default branch instead. It selects the current `main` commit, validates the
complete version and release contract, builds every candidate, and creates the
annotated tag only after all candidates pass. GitHub does not start a second
workflow for a tag created with `GITHUB_TOKEN`, so one run remains the sole
release owner.

Publication is deliberately ordered:

1. validate the exact source and version;
2. build and execute all client candidates;
3. build untagged server digests and smoke every platform;
4. create or validate the immutable annotated tag;
5. create or verify the exact Docker version manifest;
6. stage and publish the GitHub Release with verified client assets;
7. update stable rolling Docker tags;
8. copy the already-public manifest to an optional private mirror.

The GitHub Release is not published until the exact Docker version exists. If
a late step fails, rerun the failed jobs from the same Actions run so its
verified artifacts are reused. The annotated tag records its owning Actions
run, so that run can continue idempotently without allowing a different run to
claim it. The workflow also has an explicit recovery mode for an existing tag;
it refuses to move the tag or overwrite a different versioned Docker manifest.

## Required repository configuration

Create a GitHub Environment named `release`:

- require approval from release maintainers;
- allow deployments only from `main`;
- add the Environment secret `ASTRA_RELEASE_ENVIRONMENT_GUARD=configured`;
- use this as the single publication gate after every candidate is green.

Create a second Environment named `release-snapshot` for reviewed snapshot
publishing. It should have its own approval policy and the Environment secret
`ASTRA_SNAPSHOT_ENVIRONMENT_GUARD=configured`. These guard values are deliberate
fail-closed markers, not credentials: if GitHub creates a referenced but
unconfigured Environment automatically, publication stops before changing a
tag or manifest.

Provide `DOCKERHUB_USERNAME` and a least-privilege `DOCKERHUB_TOKEN`, capable
of writing only `matrixorigin/astra`, as repository or organization Actions
secrets. Candidate jobs use them to push untagged digests for runtime smoke;
only the environment-gated publication job gives those digests a user-visible
tag. Keep `IDC_REGISTRY_USERNAME` and `IDC_REGISTRY_PASSWORD` as repository or
organization secrets when the optional mirror is enabled.

After migration, remove the unused `ASTRA_SUITE_PAT` secret and
`RELEASE_MIRROR_REPOSITORY` variable once a repository search confirms that no
workflow still references them.

Protect `refs/tags/v*` with an active tag ruleset:

- restrict updates and deletions, and disallow force updates;
- if creation is restricted too, grant bypass only to the automation identity
  used by **Release Astra** and verify that a release rehearsal can create its
  annotated tag;
- do not grant a broad bypass to ordinary repository writers.

A manually created tag cannot publish anything and cannot be adopted by
recovery, but it will reserve that version until an administrator removes it.

Repository Actions should default to read-only permissions. The release
controller grants `contents: write` only to the publication job that creates
the tag and GitHub Release.

The optional registry mirror is enabled only when both
`CONTAINER_MIRROR_REGISTRY` and `CONTAINER_MIRROR_IMAGE` repository variables
are set. Proxy and runner variables are documented in `release.yml`. Mirror
failure is reported without invalidating a public release that has already
completed.

The source tree versions `@astra/sdk` and the Helm chart, but the workflow does
not yet publish either to npm or a chart registry. Treat them as explicit
maintainer actions until dedicated verification and provenance gates exist.

Legacy releases in `matrixorigin/astra-suite` and the existing `v0.0.x` tags
are historical inputs, not publication fallbacks. The current installer reads
only GitHub Releases owned by `matrixorigin/Astra`, and recovery accepts only
annotated tags created by this unified workflow. For the first repository-owned
release, choose a new version whose tag and Docker version do not exist; until
that release is complete, latest-release installation will fail explicitly
instead of silently installing a legacy package.

## Prepare a release

1. Open a release pull request from a non-default feature branch.
2. Synchronize the workspace, client, Web, lockfile, citation, Helm, all-in-one,
   and production-template Astra versions in one reviewable change:

   ```bash
   make release-prepare VERSION=0.2.0
   ```

   The command refuses to overwrite uncommitted edits to version files,
   requires the old metadata to be internally consistent, writes the new files
   atomically, and validates the resulting version set. It does not commit,
   tag, build, or publish anything.
3. Deliberately review the pinned MatrixOne and Memoria manifest digests. Change
   them only when that compatibility set has been tested.
4. Summarize user-visible changes, migrations, compatibility impact, and known
   limitations in the release pull request.
5. Apply accurate `kind/*`, `documentation`, or `improvement` labels so
   generated release notes remain useful.
6. Run the read-only local preflight:

   ```bash
   make release-check VERSION=0.2.0
   ```

7. Run `make check`, `make test-offline`, and the integration lanes required by
   the changed boundaries.
8. Merge only after required CI and review pass on the exact version commit.

`make release-check` validates synchronized versions, installer and archive
unhappy paths, repository release contracts, workflow ownership, and
documentation links. It accepts a working-tree diff so it can run before the
release commit; it does not modify files, create tags, or publish data.

## Publish

1. Open **Actions → Release Astra → Run workflow**.
2. Select `main` in the branch selector.
3. Enter the version without a leading `v`, for example `0.2.0`.
4. Leave **recover existing tag** disabled for a new release.
5. Wait for the client and server candidate matrices to pass.
6. Review the preflight summary and approve the single `release` Environment
   gate for the publication job.

Do not create the tag manually. The workflow creates `vX.Y.Z` as an annotated
tag on the source SHA after all candidate verification succeeds.

For a public rehearsal, publish an `rc` version first. The installer ignores
prereleases when resolving `latest`, and rolling Docker tags remain unchanged:

```text
0.2.0-rc.1
```

The separate **Publish Astra Docker Snapshot** workflow is for immutable,
non-semantic snapshots from the current `main` head. It rejects feature-branch
source, semantic versions, rolling tags, and attempts to overwrite a different
snapshot. Re-running the same source and name succeeds only when the published
platform digests match exactly.

## Verify the release

Verify all of the following before announcing it:

- the GitHub Release points to the workflow-selected SHA;
- all four client archives, their sidecars, `checksums.txt`, and its checksum
  are present;
- a clean Linux and macOS machine can run the documented installer;
- `astra --version`, `astra-edge --version`, and `astra --help` work;
- the exact Docker version resolves to Linux AMD64 and ARM64;
- the all-in-one source checkout at `vX.Y.Z` uses the same Astra version plus
  the tested MatrixOne and Memoria digests;
- `make stack-setup` reaches the first successful CLI turn;
- stable `X.Y` and `latest` tags resolve to the version manifest, while a
  prerelease leaves them unchanged.

## Recover or correct a release

Prefer **Re-run failed jobs** on the original Actions run. This reuses the
candidate artifacts that already passed verification.

Use **recover existing tag** only when a previous **Release Astra** run created
the annotated tag but its original run can no longer be resumed. Recovery
rejects manual and legacy tags, verifies the recorded owner is a real
**Release Astra** run from the default branch at the same source SHA, then
validates the unchanged tag, checksums, and any existing versioned Docker
manifest. It never moves a tag or silently replaces different immutable
output. If the recorded Actions run is no longer available, publish a patch
version instead of weakening ownership checks.

Do not rewrite a tag or replace a completed release in place. Fix product or
packaging defects through a normal pull request and publish a patch version. If
an existing artifact is unsafe, mark the release as a prerelease and identify
the replacement version prominently while preparing the patch.
