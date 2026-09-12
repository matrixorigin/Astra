# Authentication

Authentication owns identity, session issuance and credential binding. Request-authorized external providers keep their existing authorization protocol; verified scoped-key providers reuse the canonical identity mapping and Astra refresh-session lifecycle.

## Identity and provenance

External principals are identified by issuer/provider and subject, never subject or email alone. Memoria's provider ID is `memoria:` followed by the SHA-256 of its normalized issuer URL. The issuer defaults to the configured API base URL; an explicitly stable `MEMORIA_ISSUER` lets administrators move the transport without changing the identity authority.

`auth_external_identities` has primary key `(provider_id, external_subject)`. The mapping resolves an Astra account; it does not replace its roles or lifecycle. JWT origin and the runtime principal retain the verified provider identity. Provider-request authorization is a different principal variant and is not granted by scoped-key login.

## One composition boundary

The auth service captures validated Memoria settings during application composition and supplies one credential resolver to login, refresh, proxy routes and memory runtime consumers. A pool setter never selects a memory transport, and explicit fixture/admin transport overrides cannot redirect the user-scoped credential.

Every user operation resolves that user's persisted scoped credential first. An existing binding remains authoritative for its Memoria owner and `none` / `read_only` / `read_write` consent, including on self-hosted deployments. Lookup failures fail closed and never change authority.

Trusted self-hosted deployments may explicitly set `MEMORIA_SELF_HOSTED_MASTER_ACCESS=1` together with `MEMORIA_MASTER_KEY` and no `MEMORIA_WEB_URL`. Only an active local account with password authentication, no scoped binding and no retained Memoria identity may use this fallback. Disconnect, account deactivation/deletion and lookup failure deny access; they never manufacture a deployment-master grant for an existing runtime. Astra binds an eligible local user to the master-key transport by the canonical Astra `user_id`. Data requests use Memoria's `Memoria-Owner` authorization scheme. Memoria validates the deployment secret but exposes a non-master principal, so ID-based reads and mutations retain atomic owner enforcement. The Server defaults to disabled and this fallback must not be activated for hosted/browser-login deployments. The self-hosted all-in-one example explicitly enables it and pins compatible Memoria 0.5.2. Existing env files are preserved; operators upgrading from the older pin must install a compatible Memoria image and explicitly enable access. Memoria v0.5.1 does not support this scheme.

The provider verifies the scoped-key API contract through `/auth/whoami`: active non-master personal key, exact owner, nonempty key ID, API version 1, scopes capability and memory-filter capability. HTTP redirects are not followed. Identity, model-provider and Memoria secrets must not be logged.

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

## Legacy migration

The old `auth_memoria_identities` table is a read-only migration source. Its missing issuer cannot safely be guessed. Administrators must set `MEMORIA_LEGACY_ISSUER` to the known original issuer, matching the configured issuer. A fresh verified login then moves that mapping to the canonical table, preserves the Astra user ID and its models/Work, replaces the credential, and revokes old sessions in one transaction. Without that assertion, login fails with 409 and legacy refresh sessions fail closed. Do not set this option when pointing an old Astra database at a different Memoria instance.

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

Browser login URLs require HTTPS except for explicit loopback development addresses. Windows passes the URL as child-process environment data, not shell source.

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

Focused coverage lives in `memoria_auth_db_it`, `memoria_auth_http`, CLI auth-flow tests and runtime consent-admission tests. It includes issuer separation, concurrent login/relink, post-write failure rollback, legacy migration, disconnect, inactive-account retention, source-configuration mismatch, read-only extraction and login discovery. Actual Windows browser launch and live OAuth callbacks require platform/deployment testing in addition to deterministic contracts.

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
