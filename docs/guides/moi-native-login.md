# MOI native login

This integration authenticates the **user's local Astra binary** to an explicitly
trusted MOI deployment. It does not replace MOI's restricted server-to-runtime
grants, and it does not require users to deploy astra-server locally.

## Client behavior

Select the deployment with the global `--api-url` option or `ASTRA_API_URL`, then
run `astra login`. The server's `/auth/methods` advertises its UC integration;
setting an API URL alone does not turn an arbitrary server into a UC provider.
Use HTTPS in deployments. HTTP is permitted only for explicit loopback development.

You may also start `astra` in your own working directory and enter `/login`.
The workbench discovers the same provider and opens the UC browser flow rather
than asking for a local username/password. `/register` uses that same website,
where new users can register. After success, identity, capabilities and the
server's default model are refreshed without restarting Astra. Workspace
selection is deferred to `astra auth workspace`; no terminal prompt competes
with the workbench for input. Escape cancels browser waiting; after credential
exchange starts, wait for its result. Changing authentication ends the previous
local conversation without deleting its saved history.

The CLI opens UC in a browser. GitHub, Google, and Email login are provided by UC,
not implemented independently in Astra. Authorization uses a public `astra-cli`
client, PKCE S256, state, nonce, and an ephemeral loopback callback. There is no
embedded client secret. Both Astra and MOI bootstraps must succeed before a new
local session replaces the previous session. MOI bootstrap runs first so that
the product account mapping exists before Astra reads the shared model policy;
this does not wait for workspace provisioning or selection.

- `astra auth status` shows a readable sign-in and workspace summary;
  `astra auth status --json` emits the versioned non-secret session state for tools.
- `astra auth workspace` chooses a workspace; `--clear` removes the local choice.
- `astra model list` lists the server-governed model offerings.
- `astra logout` revokes this native session and removes local authorization.

Workspace provisioning runs asynchronously. A user can finish login without a
workspace choice; workspace-scoped MOI operations still require an explicit
selection and the normal server-side IAM decision. Login does not perform a paid
model request or require a positive balance. Invalid default-model configuration
does reject login preparation, without replacing an existing local session.

## Credential ownership

The canonical store is `~/.moi/auth.json`; `MOI_AUTH_DIR` selects an isolated
development directory. The directory and file are private (0700/0600). Do not
print, copy into bug reports, or commit this file. It contains native user access
and refresh credentials, not Genesis/model-provider keys.

Astra owns token refresh, locking, atomic publication, and logout. moi-cli uses a
short-lived Astra credential helper over a private inherited file descriptor,
not a second refresh implementation. Put the matching Astra binary on PATH or
set `MOI_ASTRA_BINARY` to its absolute path. Commands freeze account, environment,
and login generation; an account switch cannot silently redirect an in-flight
operation. An uncertain refresh rotation requires a new login, not refresh replay.

Cloud preference, outbox and resume traffic use the same selected native
endpoint and generation-bound refreshing credential as chat. A conflicting
`ASTRA_API_URL` cannot redirect that credential, and omitting the environment
variable does not disable native cloud synchronization. Legacy profiles retain
their existing endpoint selection.

The browser callback renders a self-contained result page rather than raw JSON.
Login preparation has a five-minute deadline; after credentials are durably
published, browser delivery or bounded revocation of the superseded session
cannot turn the successful login into a timeout. Failed old-session revocation
is logged and that session remains subject to UC's normal expiration policy.

A refused/DNS connection before the refresh request is sent remains retryable;
an interrupted response or rejected rotation does not. This includes HTTP 5xx:
a proxy failure or an issuer error after committing rotation does not prove the
old refresh token is reusable. Fresh-token requests only read shared state;
only expired or pending sessions wait for the rotation lock. Credential I/O and
filesystem lock waits run off the async runtime. `MOI_AUTH_DIR` must come
from the launching shell, not a workspace `.env`. Normal commands stop before
using credentials if loading `.env` changes that selection.

Deleting a session through the CLI also removes that session's local journal
and causal-state cache in the active profile, after the server confirms deletion
and while holding the session execution lease. This is irreversible local
history removal, not logout or archive. A rejected remote deletion leaves the
local history intact; a local cleanup failure is reported rather than claiming
both sides were removed. Other profiles' sessions are not removed.

Native logout remains selected locally and must not fall back to an old PAT.
That saved selection also applies to `astra login`, even after logout: if the
server stops advertising UC or returns discovery 404, login fails instead of
switching to Memoria/password authentication. A normal `~/.moi` directory without
`auth.json` does not select MOI or block legacy login merely because its permissions
are not 0700; existing credential stores still require private permissions, and
linked roots, unreadable or corrupt state are errors rather than legacy fallbacks.
UC's browser session and other native sessions are not logged out. Explicit
legacy `--profile` selection retains the existing Memoria/password/Docker path.
This also applies to `astra --profile personal login`: it bypasses UC discovery
and signs in through the legacy provider into that profile, even when the server
also advertises UC. Omit `--profile` to use MOI native login.
Existing identities and history are not merged with UC identities. Built-in MOI
memory is a server-managed capability, not another end-user login.

## Server configuration

`ASTRA_UC_MEMORY_ENABLED`, when present, accepts only `0` or `1`. Other values
(including `true` or an empty string) are configuration errors, not silent
disablement.

All configuration below belongs to the deployed Astra Server, **not the user's
local CLI**. Enabling `ASTRA_UC_ISSUER` requires all remaining settings:

| Setting | Meaning |
| --- | --- |
| `ASTRA_UC_ISSUER` | Canonical UC OIDC issuer |
| `ASTRA_UC_ADAPTER_URL` | Trusted UC backend origin |
| `ASTRA_UC_CLIENT_SECRET` | Confidential `astra-api-rs` resource-service credential |
| `ASTRA_MOI_API_URL` | Product API base advertised to native clients |
| `ASTRA_GENESIS_URL` | Genesis service origin |
| `ASTRA_UC_MEMORY_ENABLED=1` | Explicitly enable MOI built-in memory for UC users; otherwise disabled |

### Built-in memory

When enabled, also configure `MEMORIA_BASE_URL` and `MEMORIA_MASTER_KEY` on the
Server, without a Memoria login website. Do not enable
`MEMORIA_SELF_HOSTED_MASTER_ACCESS` as a workaround for UC accounts: that is a
separate, unchanged local-password policy.

The existing memory credential resolver checks the persisted mapping to the
configured UC issuer and the active local account, then checks UC account status
online. Missing/ambiguous mappings, retained disconnected Memoria identities,
disabled/deleted accounts and authority-service errors fail closed. Existing
scoped Memoria consent takes precedence. The memory namespace is `uc_` followed
by the base64url SHA-256 of issuer, a NUL separator and subject; it fits Memoria's
64-byte owner contract without truncation or cross-issuer account merging.

Recall, explicit tools, HTTP memory routes and background writes use that same
resolver. Requests use `Authorization: Memoria-Owner ...` and the resolved owner,
never an unrestricted Bearer master credential. The deployment key remains on
the Server; clients do not receive or configure it. Memoria must support this
owner-scoped scheme (0.5.2 or a verified compatible image). Old images fail
closed; do not silently retry with the Bearer master scheme.

Automatic prompt recall shares one 750 ms budget across authorization and
retrieval. Failure contributes no memory evidence and does not prevent the model
turn. Explicit writes still report their real failure; no success is fabricated.
The UC extension does not authorize MOI-managed runtime grants as native users.

The UC deployment must register the native public client, resource audiences,
scopes, and separate server-side introspection client. This is not an arbitrary
OAuth issuer adapter. Native credentials are checked online by UC, including
session revocation and account status. No positive authorization cache bypasses
these checks. Local model/provider proxy settings do not redirect UC credential
or internal service requests.

Server-side Genesis access uses the canonical UC identity and a server-only
runtime PAT. The model catalog comes from
`/api/v1/taas/llm/model-offerings`, preserving Genesis visibility and entitlements.
Astra intersects those candidates with MOI's existing `agent_chat` service-slot
policy, read from `GET {ASTRA_MOI_API_URL}/aistudio/model-policy` using the
server-owned runtime PAT in `X-API-Key`. This authenticated, read-only product
configuration endpoint requires neither a workspace nor an administrator role;
it is not a grant to workspace resources or model inference.

The allowed names and explicit default are managed only through MOI's existing
AI Studio model-selection administration. `ASTRA_GENESIS_DEFAULT_MODEL` has been
removed; there is no Astra-specific model allowlist or default override. The
Catalog TOML convention defaults only seed never-configured slots; changing that
file does not overwrite an existing administrator selection.

Every catalog lookup and inference admission reads the current policy. Removing
a model blocks its next admission, including an old session's saved Offering;
changing the default affects subsequent default resolution, not an explicitly
selected model. An unavailable, cleared or malformed policy never falls back to
the entire Genesis catalog. An unavailable default is not silently replaced by
another model. Account entitlements may narrow the list further. No model key is
returned in the catalog or stored on the user's machine. Legacy Memoria/password
and Docker/self-hosted model selection is unchanged.

An HTTP 402 inference failure is `payment_required`, not an expired login or a
retryable outage. Known billing-owner reasons (`insufficient_credit` and
`monthly_spend_limit_exceeded`) are retained without echoing arbitrary provider
response text. Streaming and non-streaming adapters do not retry 402.

## Validation boundaries

Unit tests cover native credentials, browser callbacks, online identity checks,
per-request bearer resolution, and billing errors. Matrixflow's isolated
`scripts/uc-sso-v2/native-cli-local.sh` profile and protocol/binary test scripts
exercise real UC/Keycloak, Astra, MOI, Genesis, and Billing, with a synthetic model
upstream. Existing MOI GitHub/Google/email services are platform prerequisites,
not independently reimplemented or reaccepted here. Paid-model behavior and
target-platform browser acceptance remain separate integration checks.
