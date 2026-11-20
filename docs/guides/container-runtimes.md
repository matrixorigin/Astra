# Docker and native Podman

Astra defaults to Docker Engine with Docker Compose v2. Select
`ASTRA_CONTAINER_ENGINE=podman` to invoke `podman` and `podman-compose`
directly. This path does not require Docker, `docker-compose`, `podman compose`,
`DOCKER_HOST`, or a running `podman.socket`.

## Prerequisites

Both paths require Make, Bash, Python 3.9+, OpenSSL, and the database client used
by the existing database bootstrap. Docker needs Compose v2 with `--dry-run`.
Native Podman needs Podman 4.6+ with `podman-compose` 1.6.0+ and a working network
with container DNS. The initial native validation platform is Linux. Run
commands in the host terminal when an agent sandbox cannot access its runtime.

Preflight reports missing tools before starting containers. A `docker`
executable wrapping Podman is rejected: select Podman explicitly. There is no
automatic fallback between engines.

## Select an engine

```bash
make stack-setup
ASTRA_CONTAINER_ENGINE=podman make stack-setup
```

The selector also applies to stack management, development dependencies, and
the existing `dev-api-docker-*` targets. Those targets keep their names but use
the selected engine.

For development and deployment validation, build the current checkout:

```bash
ASTRA_CONTAINER_ENGINE=podman MEMORIA_EMBEDDING_PROVIDER=mock make stack-start-head
```

`stack-start-head` builds the canonical Dockerfile from checked-out HEAD,
including local source changes, then starts and verifies the ordinary stack.
It tags the image `localhost/astra:head-<full-commit-sha>-<build-id>` and saves that reference
in the selected `STACK_ENV` file after the build succeeds. Each build gets a
new identity so Compose recreates the API even when HEAD is unchanged. Later `stack-up` and
`stack-setup` commands retain it. Rerun `stack-start-head` after changing source
or updating HEAD. Build metadata records the revision and whether the source
tree has tracked changes. Use `make stack-build-head` to build and save the
selection without starting services. Docker uses the same targets with
`ASTRA_CONTAINER_ENGINE=docker`.

The native rebuild regression can use a small local base image instead of
building the complete Astra runtime:

```bash
ASTRA_TEST_NATIVE_PODMAN=1 ASTRA_TEST_PODMAN_BASE_IMAGE=docker.io/library/ubuntu:24.04 \
  python3 scripts/dev/test_container_runtime.py RuntimeContract.test_same_head_rebuild_updates_the_running_native_image
```

It requires an already-present base image, builds two different images at the
same HEAD, checks each running container's image and file contents through
native Podman Compose, and removes its temporary project and images.

For API-only development with an existing dependency stack, build with
`make dev-api-docker-build IMAGE_NAME=localhost/astra IMAGE_TAG=head`, then run
`ASTRA_IMAGE=localhost/astra:head make dev-api-docker-up` with the same engine.

Selection precedence is an explicit environment/Make value, the selected env
file's `ASTRA_CONTAINER_ENGINE`, then `docker`. New stack env files record the
selection. Existing files and image selections are preserved. Recovery
commands include the selected descriptor and engine.

Engine stores are separate: changing a selector does not migrate containers,
volumes, or data. Use separate descriptors and explicit log directories for
separate installations. Podman defaults to its own per-project log paths and
maps dependency/init container users to the host user in rootless mode. Astra
Server retains the image's non-root identity.

Native launches do not automatically inherit host proxy variables. Set proxy
variables explicitly in the service configuration when required. Local health
probes bypass proxies. API env-file changes participate in the provider's
configuration fingerprint, so rerunning setup cannot reuse stale settings.

## Start automatically after reboot

On Linux, use the [user systemd unit](../../deployment/systemd/astra.service)
to start the configured native Podman stack. It invokes the canonical
`make stack-up` and `make stack-down` commands, including database bootstrap,
dependency ordering, and readiness checks. Shutdown preserves named volumes.
Boot uses the image saved in the stack descriptor; rebuild with
`make stack-start-head` after updating source.

The unit expects Podman's packaged
`podman-user-wait-network-online.service`, which waits for the system network
from a user service. Prepare the stack with `make stack-start-head` first,
then copy the unit:

```bash
mkdir -p "$HOME/.config/systemd/user"
cp deployment/systemd/astra.service "$HOME/.config/systemd/user/astra.service"
```

Set `WorkingDirectory` in the copied unit to this checkout's absolute path.
It uses `deployment/all-in-one/.env`; for another descriptor, append the same
absolute `STACK_ENV=/path/to/stack.env` argument to both Make commands.
Keep credentials in the owner-only stack descriptor.

```bash
systemctl --user daemon-reload
loginctl enable-linger "$USER"
systemctl --user enable --now astra.service
```

User lingering starts enabled user services at boot and keeps them running
after logout. See the [Podman startup documentation](https://github.com/podman-container-tools/podman/blob/main/docs/tutorials/socket_activation.md).
If local policy requires administrator authorization for lingering, run
`sudo loginctl enable-linger "$USER"`.

```bash
systemctl --user status astra.service
systemctl --user restart astra.service
journalctl --user -u astra.service -b
```

`active (exited)` is expected: the unit completes after stack readiness, and
Podman applies the containers' restart policies. Use `make stack-status` for
individual container health. Manage the stack through systemd while the unit
is enabled. Disable boot startup and stop the stack with
`systemctl --user disable --now astra.service`; its data is retained.

## Production and operations

```bash
# Populate .env.production with external MatrixOne and Memoria settings first.
# Rootless deployments normally need an unprivileged ingress port.
ASTRA_CONTAINER_ENGINE=podman ASTRA_PUBLIC_HTTP_PORT=8080 \
  ./scripts/ops/deploy.sh 2

ASTRA_OPS_ENV_FILE=deployment/all-in-one/.env ./scripts/ops/health_check.sh
ASTRA_OPS_ENV_FILE=deployment/all-in-one/.env ./scripts/ops/backup.sh
```

`ASTRA_OPS_ENV_FILE` defaults to the repository's `.env`. Ops select the engine
from that file, scope discovery to `ASTRA_STACK_NAME` when present, and reject
ambiguous local database matches. If the selected image has no client binaries,
ops use installed host MySQL/mysqldump tools against the same configured host
and port, with the canonical TLS negotiation policy. Passwords use the process
environment, never command arguments. Restore retains its explicit confirmation.

Production retains configuration validation, external dependencies, resource
limits, and TLS requirements. Image publishing and Kubernetes are independent
of this selector.

## Monitoring and direct Compose commands

The shared entrypoint applies engine selection and project isolation to the
checked-in Compose profiles:

```bash
cd deployment/monitoring
cp .env.example .env
# Set GRAFANA_ADMIN_PASSWORD and optionally ASTRA_CONTAINER_ENGINE=podman.
../../scripts/lib/container_runtime.sh --env-file "$PWD/.env" \
  --file "$PWD/docker-compose.yml" compose up -d
```

Use the same prefix with `compose ps`, `compose logs -f`, or `compose down`.
Direct commands remain available: `docker compose` for Docker and
`podman-compose` for Podman. The shared entrypoint adds Astra's rootless
settings and common readiness handling (`up --wait --wait-timeout 180`).

Use the shared entrypoint for the all-in-one and development dependency
profiles with Podman. It runs the canonical initialization jobs synchronously
and applies two small dependency overlays: Podman's native `--requires` expects
running dependencies and cannot represent successfully completed init jobs.
The long-running dependency health gates are retained.

Inside Podman containers, use `host.containers.internal` to reach a host-local
model endpoint. Docker uses `host.docker.internal`. Container `localhost`
refers to that container. The existing monitoring host alias is a network
mapping, independent of the container CLI/API.
The host service must listen on an address reachable from the container's
network; loopback access depends on the rootless network configuration.

## Verification

```bash
# Offline: no container engine or provider credentials required.
python3 scripts/dev/test_container_runtime.py

# Running stack: public readiness and a dependency memory round trip.
make stack-verify
```

Verification reports dependency and ordinary-user memory coverage separately.
Use mock embeddings and isolated volumes for container smoke tests; real
provider configuration belongs in the setup wizard.
