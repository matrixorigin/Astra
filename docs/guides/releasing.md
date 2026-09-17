# Releasing Astra

This guide defines the maintainer workflow for publishing Astra from the
`matrixorigin/Astra` repository. One protected workflow owns the source tag,
GitHub Release, client archives, public server image, and rolling Docker tags. A release is selected once, verified as one candidate
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

Stable promotion is monotonic within the unified workflow: publication reads
all published GitHub releases immediately before promotion, independently
checking the global `latest` and the `X.Y` release line. Recovering `0.2.3`
after `0.2.4` does not change either alias; publishing `0.2.9` after `0.3.0`
may advance `0.2` but never `latest`. GitHub's latest-release designation uses
the same global check. An unreadable or malformed release inventory fails
closed. A well-formed inventory containing an unrecognized published stable tag
(including a prerelease tag incorrectly marked stable) instead produces a
warning and disables both rolling Docker aliases and GitHub latest promotion;
verified fixed-version publication may still finish. Same-version retries
remain idempotent. Manual tag/registry changes
outside this serialized workflow are not protected by this check.

PR CI builds the Docker `dependency-inputs` stage, which is also the real
builder's base stage. It restores the cargo-chef skeleton and performs full
locked Cargo dependency resolution, including excluded and newly added path
patches. This uses the real COPY/.dockerignore inputs rather than relying on
the planner's no-deps metadata or a crossterm-specific filename assertion.
Docker validation and lint run in parallel; the existing `check` name is a
small aggregate that requires both applicable gates to succeed.
Full multi-platform image compilation and
runtime smoke tests remain release/IDC gates. Linux client builds refresh only
Ubuntu package sources for musl, retaining package verification and fatal
errors for Ubuntu source or installation failures.

Vendored terminal unit tests use a committed standalone lockfile. A separate
Linux/macOS PTY lane executes Astra's `tui::terminal_startup` tests under both
the default reader and `crossterm/use-dev-tty`, using separate Cargo calls so
features cannot be unified across the two runs. Vendor changes explicitly
select downstream Rust scopes without relying on the unknown-path fallback.

## Why publication starts from a workflow, not a tag push

Tag-triggered workflows execute release logic stored with the tagged commit.
Allowing any historical commit reachable from `main` to initiate publication
therefore lets obsolete automation become the release control plane.

The **Release Astra** workflow is manually dispatched from the protected
default branch instead. It selects the current `main` commit, validates the
complete version and release contract, builds every candidate, and creates the
annotated tag only after all candidates pass. The release workflow has no
tag-push trigger; its annotated tag records the sole release owner. Tags created with
`GITHUB_TOKEN` do not trigger additional tag-push workflows.

Publication is deliberately ordered:

1. validate the exact source and version;
2. build and execute all client candidates;
3. build untagged server digests and smoke every platform;
4. create or validate the immutable annotated tag;
5. create or verify the exact Docker version manifest;
6. stage and publish the GitHub Release with verified client assets;
7. update stable rolling Docker tags.

The protected publication job uses the built-in `GITHUB_TOKEN`. Immediately
before creating a new tag, it fetches the default branch and requires the
selected source to remain reachable from `main`. Normal merges during builds
or approval do not invalidate the candidates: the tag still points to the
exact source selected at preflight, and publication reuses its verified
artifacts. If the source is no longer reachable or branch history cannot be
fetched, publication stops before creating a tag or versioned Docker manifest.

Reachability only proves that the selected source belongs to `main` history;
it does not prove that it still represents the branch's current intent. A
reverted commit remains an ancestor. Before approving publication or rerunning
older candidates, maintainers must confirm that the selected source has not
been reverted or superseded by a decision to abandon that release.

This check is not an atomic lock on `main`. GitHub token permissions and tag
rulesets still apply, including workflow-permission restrictions on a
historical source. If GitHub rejects tag creation or existing-tag recovery,
inspect the error and any partial publication; never move the immutable tag
or overwrite its assets.

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
tag.

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
controller grants `contents: write` only to the approved publication job.
That same token performs draft lookup, body preparation, staged verification,
and publication; draft visibility requires push access. No GitHub App, App
private key, or personal access token is required.

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

## Build an IDC image independently

Run **build_push_to_idc** (`build_push_to_idc.yml`) manually from `main`. Its
`source_ref` defaults to the latest `main`; set it to `moi-dev` for that
branch's latest commit, or to a full commit SHA that is contained in the
current `main` or `moi-dev` history. Other branches, tags, abbreviated SHAs,
and commits outside those histories are rejected. Select `amd64` for a Linux
AMD64 image or `all` for the same native Linux AMD64 and ARM64 matrix used by
the release workflow. This workflow does not create Git tags or GitHub Releases
and does not push images to Docker Hub.

Configure repository variables `CONTAINER_MIRROR_REGISTRY` (host and optional
port) and `CONTAINER_MIRROR_IMAGE` (full untagged repository). Store
`IDC_REGISTRY_USERNAME` and `IDC_REGISTRY_PASSWORD` exclusively
as secrets in the `idc-publication` Environment; its deployment branch policy
normally admits only `main`. Do not keep copies as repository or organization
secrets.

The reusable IDC candidate workflow declares both credential names, and its
caller explicitly maps them. Keep that mapping even though the values are
stored only in `idc-publication`: without it, the reusable jobs receive empty
credentials. The secrets are optional at the call boundary because the caller
does not enter the Environment; the build job checks them before use.

The workflow copies the established release topology: native GitHub-hosted
runners build each selected platform by digest and run the existing all-in-one
smoke test. Candidates and build caches stay in the IDC-only
`<CONTAINER_MIRROR_IMAGE>-candidates` repository. After every platform passes,
the workflow assembles the manifest there and copies only that verified,
immutable manifest to the runtime repository as
`idc-<UTC YYYYMMDDTHHMMSSZ>-<full commit SHA>-<run ID>[-amd64]`. MOI's
newest-artifact resolver therefore never observes a candidate or cache object.
The final image records the selected commit, branch or SHA, and canonical Astra
OCI source label. Formal release tags and `latest` are not changed. The workflow
controller must be the latest `main` revision admitted by the Environment.

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

1. Fast-forward a clean local `main` checkout to the merged release commit.
2. Start the protected workflow:

   ```bash
   make release-publish VERSION=0.2.0
   ```

   The command validates the synchronized version metadata, requires a clean
   checkout at the exact `origin/main` SHA, and dispatches **Release Astra**
   with recovery disabled. It does not create a tag locally.
3. Wait for the client and server candidate matrices to pass.
4. Review the preflight summary and approve the single `release` Environment
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
source, semantic versions, rolling tags, names outside the `snapshot-*`
namespace, misleading architecture suffixes, and attempts to overwrite a
different snapshot. Re-running the same source and name succeeds only when the
published platform digests match exactly.

## Verify the release

Verify all of the following before announcing it:

- the GitHub Release points to the workflow-selected SHA;
- all four client archives, their sidecars, `checksums.txt`, and its checksum
  are present;
- a clean Linux and macOS machine can run the documented installer;
- `astra --version`, `astra-edge --version`, and `astra --help` work;
- Darwin release jobs prove that a session execution lease admits one owner,
  rejects a concurrent owner, and can be reacquired after release;
- the exact Docker version resolves to Linux AMD64 and ARM64;
- the all-in-one source checkout at `vX.Y.Z` uses the same Astra version plus
  the tested MatrixOne and Memoria digests;
- `make stack-setup` reaches the first successful CLI turn;
- eligible stable `X.Y` and `latest` tags resolve to the version manifest;
  prereleases and recovery of superseded versions leave newer aliases unchanged.

## Recover or correct a release

Prefer **Re-run failed jobs** on the original Actions run. This reuses the
candidate artifacts that already passed verification.

Use **recover existing tag** only when a previous **Release Astra** run created
the annotated tag but its original run can no longer be resumed. Recovery
rejects manual and legacy tags, verifies the recorded owner is a real
**Release Astra** run from the default branch at the same source SHA, then
validates the unchanged tag, checksums, and any existing versioned Docker
manifest. Recovery skips candidate rebuilds and downloads the exact verified
client archives and server digests from the run recorded in the annotated tag.
Each digest-addressed server candidate is also retained by an immutable
`astra-candidate-RUN_ID-PLATFORM-DIGEST` Docker tag. Including the digest keeps
retries immutable even if a rebuilt OCI provenance envelope changes. Recovery
verifies both the digest object and its run-scoped tag before requesting
protected publication approval.
The owner artifacts that carry the client archives and server coordinates are
retained for 30 days; short-lived per-platform client build artifacts are not
part of the recovery contract.
It never moves a tag, silently replaces different immutable output, or treats a
new build as proof of the old release. If the recorded run or its retained
candidate artifacts are no longer available, publish a patch version instead
of weakening ownership checks.

Recovery executes the release controller and verification scripts from the
workflow revision selected on the protected default branch. The historical tag
is checked out into a separate source directory for inspection and remains an
immutable release input, not executable control-plane code. Before a
draft becomes public, its remote asset names, sizes, upload states, and GitHub
SHA-256 digests must exactly match the locally verified candidate set.
Draft release notes carry the immutable owner run and source identity. Repeated
staging preserves and overwrites the same canonical body without regenerating
notes; a manual or differently owned draft fails closed instead of contributing
text to the public release.

Client archives use the selected source commit time as `SOURCE_DATE_EPOCH` and
normalize member order, ownership, modes, paths, and gzip metadata. Rebuilding
the same binaries for the same source therefore produces byte-identical
archives; recovery still prefers the original verified artifacts rather than
depending on a rebuild.

Do not rewrite a tag or replace a completed release in place. Fix product or
packaging defects through a normal pull request and publish a patch version. If
an existing artifact is unsafe, mark the release as a prerelease and identify
the replacement version prominently while preparing the patch.
