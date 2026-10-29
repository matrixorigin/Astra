# Authentication

Authentication owns identity, session issuance and credential binding. Request-authorized external providers keep their existing authorization protocol; verified scoped-key providers reuse the canonical identity mapping and Astra refresh-session lifecycle.

External provider catalog and runtime-context requests carry the verified `provider_scope_id` and `external_subject` from the authorized principal. The adapter exposes request authorization and these scope-based operations. Current provider-request HMAC and Edge-token admission use local verification; Edge registration also checks revocation. Login, refresh and logout use Astra's canonical authenticated-session services; the retired password/session-handle adapter interfaces and their source-level exports are removed.

## Identity and provenance

External principals are identified by issuer/provider and subject, never subject or email alone. Memoria's provider ID is `memoria:` followed by the SHA-256 of its normalized issuer URL. The issuer defaults to the configured API base URL; an explicitly stable `MEMORIA_ISSUER` lets administrators move the transport without changing the identity authority.

`auth_external_identities` has primary key `(provider_id, external_subject)`. The mapping resolves an Astra account; it does not replace its roles or lifecycle. JWT origin and the runtime principal retain the verified provider identity. Provider-request authorization is a different principal variant and is not granted by scoped-key login.

## One composition boundary

The auth service captures validated Memoria settings during application composition and supplies one credential resolver to login, refresh, proxy routes and memory runtime consumers. A pool setter never selects a memory transport, and explicit fixture/admin transport overrides cannot redirect the user-scoped credential.

Every user operation resolves that user's persisted scoped credential first. An existing binding remains authoritative for its Memoria owner and `none` / `read_only` / `read_write` consent, including on self-hosted deployments. Lookup failures fail closed and never change authority.

Trusted self-hosted deployments may explicitly set `MEMORIA_SELF_HOSTED_MASTER_ACCESS=1` together with `MEMORIA_MASTER_KEY` and no `MEMORIA_WEB_URL`. Only an active local account with password authentication, no scoped binding and no retained Memoria identity may use this fallback. Disconnect, account deactivation/deletion and lookup failure deny access; they never manufacture a deployment-master grant for an existing runtime. Astra binds an eligible local user to the master-key transport by the canonical Astra `user_id`. Data requests use Memoria's `Memoria-Owner` authorization scheme. Memoria validates the deployment secret but exposes a non-master principal, so ID-based reads and mutations retain atomic owner enforcement. The Server defaults to disabled and this fallback must not be activated for hosted/browser-login deployments. The self-hosted all-in-one example explicitly enables it and pins compatible Memoria 0.5.2. Existing env files are preserved; operators upgrading from the older pin must install a compatible Memoria image and explicitly enable access. Memoria v0.5.1 does not support this scheme.

The provider verifies the scoped-key API contract through `/auth/whoami`: active non-master personal key, exact owner, nonempty key ID, API version 1, scopes capability and memory-filter capability. HTTP redirects are not followed. Identity, model-provider and Memoria secrets must not be logged.

MOI native UC deployments may explicitly enable built-in memory with
`ASTRA_UC_MEMORY_ENABLED=1`. This is a product authority, not an expansion of the
local-password fallback. The same resolver accepts only the configured UC
issuer's persisted identity, an active Astra account and a fresh active UC
account-status response. It uses the owner-scoped deployment transport, a
64-byte-compatible issuer/subject namespace, and no client-held memory key.
Retained Memoria identities/credentials prevent a disconnected scoped account
from acquiring this authority. Existing scoped consent remains authoritative.
See [MOI native login](../guides/moi-native-login.md#built-in-memory).

Memory proxy denials include additive `error_code` values without changing
authorization policy: `memory_consent_denied` for a scoped permission denial,
`memory_self_hosted_access_disabled` for an eligible unbound local account in a
deployment without a login website whose fallback is disabled, and
`memory_access_disabled` for other disabled authority. Only the self-hosted code
produces deployment-switch advice in CLI diagnostics. Revoked/inactive authority
must not be presented as a reason to enable fallback. Authentication middleware
can reject invalid or revoked sessions earlier with 401. Older servers without
these codes retain generic CLI permission hints; detail text is not parsed to
infer an authorization category.

## Ordinary request authentication cache

The authentication service verifies each local access JWT's signature, type,
subject, session, origin and expiry before consulting its process-local cache.
A miss reads account activity, refresh-session expiry and issuer binding in one
SQL snapshot. A successful observation can be reused for at most 30 seconds,
bounded further by token/session expiry and the Memoria access-token lifetime.
Hits do not extend freshness. Ordinary local authentication therefore uses one
cold statement and no SQL on a cache hit; Memoria misses additionally resolve
the current connection credential. No bearer, password hash or memory credential
is retained in the cache.

The cache is shared by service clones and bounded to 1,024 entries, 4 MiB of
accounted storage and 16 KiB per entry. Reconfiguring a service's pool, issuer or
encryptor starts a new namespace. Logout invalidates only its session's local
observations and fences matching in-flight fills; account-wide changes still
invalidate all observations, including on uncertain commits. Session fences are
bounded within the cache; overflow conservatively fences older in-flight fills.
Other processes and out-of-band account/session mutations can take up
to 30 seconds to affect ordinary request admission. Expired observations require
a successful fresh lookup; database errors do not enable stale fallback.

A successful database snapshot authenticates a request overlapping logout;
cache invalidation prevents retaining that observation and does not reject an
unrelated principal. Absolute token, observed Session and Memoria access expiry
are checked again after awaited validation. A request starting after a local
logout cannot reuse an invalidated observation.

Refresh and logout serialize on the account lock. Refresh atomically replaces
the active token hash in its existing session row; the consumed hash cannot
refresh again. Logout uses a signed, strictly unexpired refresh token's user and
session claims to revoke that session, even after the old hash is replaced or
cleaned up. A valid proof with no remaining session is an idempotent success;
it cannot revoke another session. This relies on unique session IDs and trusted
JWT signing keys: removing an old hash alone no longer revokes its logout proof.
Expired or revoked rows remain eligible for the existing bounded cleanup.
Do not mix this version with older servers: an old server may report logout
success after a new server has already replaced the hash, leaving the session
active. For upgrade or rollback, stop admission to all API replicas, drain or
terminate their in-flight requests, and wait until every old process has exited
before starting any replica of the other version. This cutover interrupts
service; ordinary rolling updates and session affinity do not provide the
required ordering. Keep all API replicas on the same side of the cutover.
After rollback, an older server cannot use a token already consumed by the
newer version to revoke its successor; it rejects that logout. Draining
requests does not restore replaced hashes, so clients must use their current
refresh tokens or the affected sessions must be revoked separately.

This cache observes identity, not operation permissions. Memory operations
continue resolving scoped credentials through the canonical credential resolver;
provider-request replay checks, sensitive reauthentication and execution-handoff
reauthorization retain their own fresh authority checks. Already admitted work
continues under its existing execution/cancellation authority.

## Atomic binding and sessions

Login verifies the key, takes the canonical account lock, rechecks the key after waiting, and commits identity, encrypted binding and refresh session together. Concurrent first logins resolve one account. A deterministic credential token primary key identifies one binding per provider/account; replacements remove superseded ciphertext in the same transaction. The verified key ID identifies the upstream key generation. Astra also persists a local connection lifecycle nonce in credential metadata: ordinary login with the same active binding preserves it; key replacement or reconnect after disconnect generates a new nonce, even when the upstream key ID is unchanged. Failed issuance rolls back every login-owned row.

Refresh validates the current Memoria credential online. Compare-and-revoke of the old refresh token prevents a concurrent disconnect from resurrecting its session. Access TTL is bounded to 15 minutes. Runtime consumers independently resolve current consent and deny inactive/missing accounts.

## Disconnect and retention

- Logout revokes one Astra session. It does not revoke other devices or turn off memory sharing.
- Disconnect removes Astra's stored Memoria credential and revokes the account's Astra refresh sessions atomically. It retains identity mapping, models and Work so explicit relinking recovers the same account.
- Disabling memory sharing is different: an identity-only key remains usable for sign-in.
- Memoria owns upstream key revocation and deletion of its accounts/memories. Astra disconnect must not claim to perform those actions.
- Deactivated/deleted Astra accounts cannot resolve their scoped credential. Bounded maintenance removes their stored ciphertext. Existing external mappings do not automatically recreate a deleted local account.
- Account-wide Work/history erasure remains governed by the account retention contract; sign-out, disconnect and temporary verification outages never erase it.

## Current-schema identity policy

Fresh schema bootstrap creates `auth_external_identities`. It does not import
`auth_memoria_identities` or migrate an older database; unsupported schema
versions require database recreation.

Login resolves only the current verified issuer and subject. There is no
issuerless identity adoption or relink migration. Retained Memoria identities
and inactive credentials still deny deployment fallback; an absent current
credential is not permission to change identity authority.

Each resolution reads account state, scoped credentials, retained bindings and
configured UC identity from one database statement snapshot. This snapshot is
not cached: each memory operation rechecks current authority, and UC memory
still verifies account status upstream. Inapplicable compaction performs no
credential lookup.

## Fresh reauthentication

Normal sign-in is separate from authorizing device trust, device re-enrollment,
or forced session takeover. `GET /auth/reauthenticate` is authenticated discovery:
local accounts receive `method: password`; Memoria accounts receive
`method: memoria` and a verification page under the configured `MEMORIA_WEB_URL`.
This URL is a trusted identity-verification endpoint, not a caller-selected URL.
It requires HTTPS except for explicit loopback development.

The website requires a fresh email code sent to the current account's email.
This also works for GitHub/Google-created accounts with an accessible email;
an existing OAuth login session by itself is not fresh verification. Private
API-key login deployments keep their existing local-password reauthentication.
The email names the sensitive action. Codes are purpose/account/key-generation
bound, expire in five minutes, allow five attempts, and have a 60-second resend
cooldown. Code hashes use an application-keyed HMAC. Normal login codes cannot
be used here. Verification neither rotates the connection key nor changes
memory consent.

Successful verification returns an opaque `msu_` proof, valid for two minutes.
The user copies it back to the originating action. It stays out of URLs,
browser storage, and logs. The SDK accepts
`reauthenticate({ memoriaProof }, purpose)`; the HTTP request is
`POST /auth/reauthenticate` with `{ "memoria_proof": "msu_…", "purpose": "device_trust" }`.
The existing password request remains supported for local accounts. Mixing
methods is rejected.

Astra validates the active issuer/subject/connection generation online, then
consumes the website proof over the fixed HTTPS backchannel
`/api/auth/astra/reauthentication/consume`, with redirects disabled and a bounded
response. It checks the returned subject, key generation, purpose and timestamps
and revalidates its binding before issuing the existing five-minute `rp_` proof.
That proof is single-use, purpose-bound and additionally bound to the Memoria
identity, upstream key generation and Astra connection lifecycle. Disconnect
deletes pending proofs in the same account-locked transaction as the binding.
The lifecycle binding also prevents an in-flight proof issuance from becoming
usable after reconnect with the same upstream key. Legacy credential metadata
without a lifecycle nonce remains readable; the next login assigns one. Proofs
issued before this binding-format upgrade must be obtained again (their maximum
lifetime is five minutes). Device trust still requires the separate
device-possession challenge. Both proof exchanges fail closed on upstream
revocation, disconnect, identity mismatch or replay; failed exchanges require
fresh evidence rather than bypassing proof checks.

The website must be deployed with the reauthentication API before Astra clients
can use this path. An unavailable verifier blocks only the sensitive operation,
not ordinary sign-in or chat. Astra does not need a Memoria master key or a new
shared signing secret.

## Client/deployment contract

`GET /auth/methods` advertises the Server's website and issuer. An unset website retains interactive password login, including all-in-one deployments. The CLI falls back to the older password journey only on discovery 404, not on outages or malformed configuration. Explicit username/password and manual scoped-key login remain available.

`astra login` and the workbench `/login` share one discovery decision: UC when
advertised without an explicit legacy profile, otherwise Memoria browser login,
otherwise an explicitly enabled password form. `/register` in the workbench
uses the same decision; hosted registration belongs to the discovered website,
not a local password form. Discovery errors never select password authentication.
The workbench remains responsive during discovery and browser waiting. Escape
can cancel before credential exchange starts; once exchange starts it awaits
the bounded result instead of claiming cancellation after a possible commit.
Browser authentication closes the previous local conversation before credentials
are replaced. Successful login binds the identity and refreshing transport,
resynchronizes capabilities and resolves the server default model in the current
workbench, without requiring a manual restart. Browser waiting and cancellation
retain the previous identity-pinned Edge heartbeat. On completion, the workbench
stops and joins it before rebinding local execution. Login completion registers
the current checkout with the new identity before reporting chat readiness,
then starts its heartbeat; registration failure is reported explicitly and does
not initialize a ready runtime. Existing transports retain their old generation
binding; they cannot silently adopt another account. Authentication
changes are refused while local background tasks are running.

At browser re-login the CLI drains and retires its old session-memory service
before credential publication. Successful completion rebuilds that service with
the authenticated identity, including when the process started signed out. It
also replaces the remote skill registry and its private cache, restarts external
discovery, and attaches both the workbench and runtime to the new registry.
Same-account re-login still replaces these clients because the credential
generation changed; previously created transports remain generation-pinned.
Cancelling browser waiting restores the previous identity's memory service and
external discovery through the same completion path as an unsuccessful login.
Native CLI memory proxy operations use the selected environment's endpoint and
refreshing generation-bound credential together, ignoring conflicting legacy
URL configuration and refusing HTTP redirects. Legacy memory proxy routing is
unchanged when no native identity is bound.

Browser login URLs require HTTPS except for explicit loopback development addresses. Windows passes the URL as child-process environment data, not shell source.

Terminal and workbench login share an asynchronous browser-opening boundary.
On WSL, command-local `WSLENV` forwards the URL unchanged to one PowerShell
invocation; the URL never becomes script source. Its completion is observed
for up to five seconds, with one second for local cleanup. Failure or timeout
produces a sanitized warning while the displayed manual URL and callback stay
usable. There is no alternate automatic launcher. Failed or cancelled attempts
signal their POSIX process group and collect the direct child. Owner destruction
synchronously signals that group, even during runtime shutdown when asynchronous
collection cannot run. This does not prove Windows-process termination or revoke
a browser request already in flight.
Successful handoffs and native desktop browsers retain their own lifetime.
Workbench cancellation still vetoes credential exchange through `LoginControl`.
Observers do not wait for processes, and opening results do not authorize login.

### Browser-delivered local authorization codes

The CLI binds its existing listener to `127.0.0.1:0`, generates a fresh state
and private verifier, and opens the discovered website's `/connect/astra` with
`port`, `state`, `cli_version`, `callback_transport=authorization_code_v1`,
`code_challenge_method=S256`, and SHA-256 `code_challenge`. The verifier never
enters the browser URL, logs, or profile storage. No anonymous start endpoint
or remote approval polling exists.

The signed-in browser shows the account and the existing single confirmation
button. There is no code to enter or compare and no additional checkbox.
Confirmation calls the website's authenticated
`POST /api/integrations/astra/browser-login/authorize`. It reuses the canonical
integration-key/consent owner and returns only a random, one-time authorization
code with a 60-second lifetime, never a connection key or Astra session token.

The browser performs a **top-level GET navigation** to
`http://127.0.0.1:<port>/callback?code=...&state=...`. This is a native-app
loopback redirect, not cross-origin fetch, an iframe, a popup, or an insecure
form POST. The destination is constructed from a validated integer port; no
caller-provided hostname or arbitrary redirect URL is accepted.

The local listener checks the state and bounded, unambiguous request, then
redeems the code once through
`POST /api/auth/astra/browser-login/redeem` on the discovered website. The
request carries the code, private verifier, exact redirect URI and state.
The website checks S256, expiry, active account, and the exact active
integration-key generation under the canonical account lock. A conditional
consume admits one winner. Rotation, revocation, inactivity and replay fail
closed. Only code hashes and binding metadata are stored in the additive
`srv_astra_login_codes` table.

Security boundary: transferring the initial website URL to another computer
does not deliver its browser's code to the remote initiator. A public challenge,
state, port, or even the initiator's private verifier alone cannot retrieve
a code or credential. The remote initiator has no polling endpoint. A local
process that intercepts the callback still lacks the private verifier. This
does not defend against a compromised local host or a user deliberately
forwarding the final secret authorization code.

After redemption, the CLI uses the existing `/auth/memoria` exchange and saves
the existing Astra profile tokens. Only then does the local page report
successful login. No Astra Server schema, identity mapping or session-lifecycle
change is needed.

The CLI's GET authorization-code callback serves a self-contained HTML card:
200 on successful login, 400 on invalid callbacks or failed exchange. A malformed
request whose prefix identifies a GET `/callback` navigation also receives the
failure card, including oversized cookie headers; unknown/malformed methods
receive JSON errors. Legacy POST callbacks remain JSON (including valid JSON
error bodies), and OPTIONS remains an empty 204 response. Unrelated GET paths
return a small 404; `/favicon.ico` returns an empty 204, not a login failure card.
Responses use `no-store`, `no-referrer`, and `nosniff`. CSP blocks scripts,
external resources, base URLs, form submissions and framing. The HTML response
authorizes only its embedded stylesheet by an exact SHA-256 hash. Only static
application copy enters the page, never callback fields, identity, credentials
or upstream error text. There is no close button or additional approval step.

The card references the website's AstraConnect layout and palette, but is not a
pixel-identical copy. Intentional differences include the Astra Cloud label,
terminal-specific instructions, no interactive controls, a 600-weight deep-gray
heading, higher-contrast dark-mode secondary text (`#94a3b8`) and a darker
light-mode success icon (`#047857`). Do not reduce contrast to match website
tokens. It follows the system color scheme and locally available fonts, including
`system-ui`; the loopback origin cannot read the website's saved theme or load
its web fonts. The standalone assets are
`crates/astra-cli/src/cli/auth_flow/result_page.html` and
`crates/astra-cli/src/cli/auth_flow/result_page.css`. They are maintained separately
from the website and need visual comparison when either design changes. CSS
changes need no manual CSP update: the hash is calculated from embedded CSS at
runtime. Both assets use LF even on Windows to avoid browser newline
normalization changing the hashed style text. Only the CLI binary needs updating.

Website HTTPS is required except explicit loopback development URLs. Code
exchange does not follow redirects, bounds response size and duration, and is
not automatically replayed after failure: consumption may already have
succeeded. Invalid/mismatched GET callbacks do not trigger token exchange.
The five-minute local listener deadline remains in effect, including response
writes and both exchange paths. Each response write/shutdown is additionally
bounded to two seconds; delivery errors are reported without sensitive contents
and never undo a persisted login. Three invalid callback attempts end the login;
favicon, OPTIONS and unknown GET paths do not consume that rejection allowance,
but an overall 64-request allowance bounds unrelated traffic too. These bounds
contain resources; they do not guarantee availability against a hostile local
process, which can also exhaust the existing invalid-attempt allowance.

Compatibility is explicit in the CLI link. An old website can ignore the
capability fields and use the unchanged JSON POST callback, which still checks
exact website Origin, state, method and content type. There is no speculative
start call and no 404/405/HTML-based fallback decision. A CLI-only update does
not fix Safari against an old website; upgrade website backend and frontend
before distributing the new CLI. Old CLI links remain supported. Explicit
password and manual-key login remain unchanged.

## Verification

Focused coverage lives in `memoria_auth_db_it`, `memoria_auth_http`, CLI auth-flow tests and runtime consent-admission tests. It includes issuer separation, concurrent login/relink, post-write failure rollback, disconnect, inactive-account retention, source-configuration mismatch, read-only extraction and login discovery. Actual Windows browser launch and live OAuth callbacks require platform/deployment testing in addition to deterministic contracts.

`memoria_live_contract_it` additionally runs against a real Memoria API implementing scoped-key API version 1. It creates disposable identity-only, read-only and read-write keys, verifies stable Astra account mapping, and revokes each key through Memoria before checking refresh rejection. Run only against an isolated Memoria test service and an explicitly designated test database:

```bash
# Set the isolated MatrixOne connection variables as in the DB testing guide.
export ASTRA_TEST_DB_IT=1
export ASTRA_TEST_DATABASE=review_memoria_contract
export ASTRA_TEST_MEMORIA_URL=http://127.0.0.1:18104
# Set ASTRA_TEST_MEMORIA_MASTER_KEY to the isolated Memoria service's test key.
make test-memoria-auth-online-contract
```

The test issues and revokes upstream keys; never point it at a production service.

## MOI device authorization

`astra login --device-code` is an explicit native UC alternative for terminals
without a browser. It shares discovery/environment validation, nonce-verified
ID tokens, product preparation, atomic login publication, refresh and logout
with the existing UC browser login. It has no new local credential schema.
Discovery's device endpoint is optional; its absence leaves browser login
unchanged and rejects only the explicit device command.

UC admits the fixed `astra-cli` scope set and S256 PKCE request. The CLI prints
the manual verification URI and user code, never the grant secret or a complete
verification URI. Keycloak owns grant state and the final consent. UC's typed
identity continuation must return to the original Keycloak device session,
without opening a fresh authorization-code flow. The UC implementation and
rollout are tracked in [Matrixflow #17972](https://github.com/matrixorigin/matrixflow/issues/17972).

Polling obeys the issuer interval, increases it permanently for `slow_down`,
backs off proven connection failures, and
stops at a bounded wait of `min(expires_in, 600)` seconds. Longer server lifetimes remain valid;
the local wait limit is reported separately from server grant expiry. No final
poll bypasses the issuer interval or retry delay. Authorization creation errors
distinguish HTTP 429, HTTP 5xx and rejection without rendering upstream bodies.
A token request with an unconfirmed outcome is terminal and is not replayed,
including ambiguous transport failures, HTTP 429/5xx, and
`temporarily_unavailable` responses. New credentials are
published only after existing MOI and Astra preparation succeeds. No implicit
browser/manual fallback is introduced.

An in-flight token request gets an independent bounded settlement period after
the local deadline or cancellation. An unconfirmed response is reported as such,
rather than claiming the grant expired. The shared native credential owner
arbitrates cancellation against rename admission, polls contended publication locks for up
to ten seconds, and distinguishes pre-rename failure from visible publication
whose directory durability is unconfirmed. Callers join the writer after
cancellation, revoke new credentials only on definite pre-commit failure, and
attempt old-session revocation for both durable and visible-but-unconfirmed
publication. Filesystem operations themselves cannot be forcibly interrupted;
no caller timeout may leave a detached writer behind. This publication contract
is shared by browser and device login; there is no device-only auth.json writer.
Optional workspace selection uses the same bounded publication owner after login.
Refresh sends the one-time token only after its pending intent is durably synced;
a visible replacement with unconfirmed directory durability is never revoked.

## CLI in-flight credential ownership

A CLI turn captures its admitted credential owner once, including when
`--profile` is omitted and the stored default is selected. Tool result and
approval acknowledgements, exact-run cleanup, wall-deadline cancellation and
user cancellation resolve credentials through that captured binding. One
reactive 401 refresh may repost the same callback or cancellation; it does not
re-execute a tool or resubmit an admitted turn. Only typed model-catalog
preflight authentication failures without admitted-run or effect evidence may
retry turn admission.

Stdio captures that owner before Edge registration can refresh its bearer,
then resolves the usable admission token through the same checked binding.
Selected-model and default-model catalog failures both preserve typed HTTP
evidence. Proactive callback-auth rejection retains the callback run id and
wakes the physical SSE reader so exact-run cleanup can begin; it does not
cancel the parent/user token.

Confirmed refresh rotations within that binding remain usable. Changing the
ambient default profile does not change an in-flight owner. Logout, account or
native generation changes, and unrecognized credential replacement fail closed;
an environment bearer cannot borrow a stored profile's refresh authority.
Bearer resolution and exact cancellation share one absolute cleanup deadline.
Only `execution_settled: true` confirms execution settlement; a terminal status
alone does not release an uncertain local execution lease. Internal transport
detach retains the existing server recovery semantics and is not user cancel
authority.


## Web browser credential delivery

Web uses one HttpOnly `astra_session` selector. A browser login imports a matching
access/refresh pair through `POST /auth/browser-sessions`, consumes that refresh
JWT, and stores a stable `bs3.<runtime-root>.<user-id>.<session-id>.<random-proof>` handle in the cookie.
The runtime root, user and session hints, and 256-bit random proof are bound together.
The runtime verifies the handle's user and session IDs against the signed refresh token
before adoption, preventing a logged-out handle from being rebound to a different
session. An ordinary
request never sends this proof to a separately configured runtime URL.

`auth_refresh_tokens` owns session identity, revocation and the encrypted browser
credential pair in the same row. The handle hash is checked against the session primary key; raw proofs
are never stored. `/auth/refresh` accepts either a direct single-use JWT or the
independent browser proof. A consumed direct JWT cannot recover a successor. A
browser proof returns the current access JWT while usable; refreshing atomically
replaces the canonical hash and encrypted pair under the existing account lock.
A loser reads the committed winner. Provider verification happens before locking,
and its identity/credential binding is checked again under lock.

The ordinary browser resolution uses the random session ID as the canonical row
primary key, stored only as `session_id`, and verifies the full handle hash in one authority snapshot. Direct
refresh and principal checks use the same primary key. Refresh also compares the
signed refresh token hash on that row; the hash is not a separate index. When the
current access token is usable, ordinary browser resolution does not write or call
the provider; rotation writes and revalidates the provider when applicable. React
request caching deduplicates Server Component configuration reads. Chat,
project and sidebar handlers and hydrated pages pass their admitted client through
the request; there is no cross-user or process-local token authority. A late Stop
acknowledgement resolves authority again before reconciliation. The existing SDK owns later 401
retries, cancellation, REST and streaming admission. An initial owner/provider/DB
outage is reported as unavailable. A later SDK refresh failure retains the SDK's
existing original-401 behavior; it never clears the proof or reports success.

RSC rendering, sidebar, ordinary API calls and refresh never write authentication
cookies. Handle logout revokes the captured session and deletes its owner in one
transaction, but leaves the now-inert cookie: a delayed logout response must not
erase a newer login. Revocation uses the exact handle hash and canonical session
identity, without decrypting credentials, calling a provider or requiring an active
account. Expiry, deleted old accounts and replaced encryption keys cannot trap the
browser in a session it cannot sign out of or replace. Other replicas reject
subsequent owner resolutions. Ordinary principal checks can retain an already
admitted access JWT for the existing maximum 30-second cache interval. The
existing admin authorizer checks JWT and role independently of session revocation;
an already issued admin access JWT can remain usable until its expiry. Signing
in creates an independent session;
runtime configuration changes only the browser selection. Neither operation
implicitly revokes a previously selected session. If the response is lost, the
browser can continue using its old selection. A copied old browser handle may be
refreshed until its one-year absolute limit unless explicitly revoked. Sign out
before switching when immediate revocation is required: signing out afterwards
revokes only the currently selected session. An unreceived new session expires
through the normal retention path. Static bearer tokens must be revoked at their
issuer and can outlive the Web cookie.

Runtime settings are full replacements, encoded as `cfg1` in the same selector:
explicit static bearer, no authentication, demo, or deployment default. The deployment API URL may supply a default destination; `ASTRA_ACCESS_TOKEN`
is never a browser identity. Anonymous/default selections cannot inherit a server
credential. Each browser must sign in or explicitly supply its own static token. Refresh-pair import through `POST /api/runtime-config` requires `browserProof`: a
caller-generated independent 32-byte CSPRNG secret encoded as unpadded base64url
(for example, `crypto.randomBytes(32).toString("base64url")` in Node.js). Generate
it before importing, retain the same proof, token pair and runtime URL across uncertain retries
against any Web replica, and discard caller storage after confirming cookie delivery.
Treat the proof as a bearer secret; never derive it from tokens or log it. Missing
or malformed proof is rejected before consuming credentials. This also supports
passwordless external accounts; interactive password login obtains a fresh pair
on each retry. Raw refresh JWTs are never stored in cookies. Demo configuration disables live access; it does not authenticate a workspace.
The public landing remains available and live workspace pages require sign-in.
Demo combined with credentials is rejected as ambiguous; logout exits demo. Missing/malformed/revoked
browser credentials do not fall back to environment authentication. Static logout
selects explicit no-auth mode; settings DELETE restores the deployment destination (without credentials) without revoking the previously selected session. Explicit logout remains the way to request revocation. These settings changes, static logout
and login/import explicitly write the selector. Two concurrent explicit changes
can arrive in reverse response order: HTTP cookies do not support compare-and-set.
This limitation does not affect delayed ordinary refresh or handle-logout responses.

The encrypted payload has an immutable one-year absolute cap. Browser rotation
caps the canonical refresh-row expiry to it, so bearer admission and handle
resolution respect the same ceiling. No separate browser expiry or sweep exists.
Refresh retention treats the configured seven days as a minimum period after
expiry. Logout and provider disconnect shorten expiry to the earlier of its current value
and database time, and clear browser ciphertext in the same update. Repeated
revocation cannot postpone cleanup. Revoked rows remain for the retention period
plus any cleanup backlog. Other
ciphertext remains until expiry, the retention period and any cleanup backlog.

Refresh maintenance captures a conservative UTC cutoff, binds it as a concrete
DATETIME to the expiry index, and rechecks each candidate and the cutoff in a
primary-key batch delete. The bound candidate cutoff avoids the observed MatrixOne 4.2.5 dynamic time range
error during candidate selection. The DELETE retains a database-clock condition
and rechecks the base row; its index completeness and physical I/O remain engine-dependent. The delete checks the current database
clock and the retention period again, so a concurrent renewal or backward clock
change cannot cause premature deletion. Correct expiry selection still depends
on MatrixOne's index returning complete results. Candidate anomalies and stalled
batches are logged and retried; the maintenance worker uses bounded batches and
periodic retries without persistent coordination. Each catch-up round keeps a
fixed cutoff and a normal six-hour sweep performs one follow-up check even if
its first batch was empty. Replica conflicts can delay progress, and clock
synchronization remains a deployment requirement. An admin cleanup error may
follow already committed batches or other table deletions; retrying is safe.
JWT signing and `ASTRA_TOKEN_ENCRYPTION_KEY` must match across runtime replicas.
Missing keys, key mismatch and corrupt ciphertext fail closed. Changing either key
requires sign-in again; it is not transparent key rotation.
Web page errors offer retry and an explicit sign-in link, including when a saved
session cannot be decrypted. The standard Next error boundary displays no backend
error details and never grants an anonymous identity or changes the selector.
Authentication APIs retain their original error status. The live Work browser
harness enrolls through `POST /api/runtime-config` using its own credential and
the browser's shared cookie jar; it does not construct authentication cookies.

### Fresh-schema deployment

This change advances the strict core schema contract to `2026-10-29-v116`. It does
not migrate, alter, drop or recreate an existing database. Provision a **separate
fresh database**, preserving the old database and its data. Existing data does not
appear in the fresh database without a separately designed migration. Drain old
Web/runtime replicas before cutting over both tiers and require new sign-in; legacy
token cookies are not converted. Do not mix old cookie writers with new handle
readers. Rollback also requires a drain and the original code/database; older
runtimes cannot consume new browser handles.

Validation: Web unit/public-route tests; `browser_sessions_db_it` for real DB
concurrency, rollback, provider replacement/disconnect and isolation;
`browser_session_next_db_it` for two real Next servers and two independent runtime
auth services sharing an isolated DB. Build the current Web source before running
the latter (`npm --prefix packages/sdk run build && npm --prefix web run build`),
use the Node version pinned in `.nvmrc`, install the lockfile-matched Playwright
Chromium headless shell (`cd web && npm exec -- playwright install --with-deps chromium --only-shell`),
and explicitly set `ASTRA_TEST_DB_IT=1` and `ASTRA_TEST_DATABASE`. The test also
corrupts disposable owners and drives the real browser recovery links, including
fresh document retry and sign-in across the two Web replicas.
No live provider or LLM credentials are needed.
