#!/usr/bin/env node
/**
 * Mode B quick smoke: GET {base}/health, optional /auth/me, /sessions, run routes.
 * Uses global fetch only (no SDK build). Mirrors env vars for Jest Mode B.
 *
 * Usage:
 *   ASTRA_SDK_BASE_URL=http://127.0.0.1:8080 node scripts/sdk-online-smoke.mjs
 *   ASTRA_SDK_BASE_URL=... ASTRA_SDK_ACCESS_TOKEN=... node scripts/sdk-online-smoke.mjs
 *   ASTRA_SDK_PATH_PREFIX=/api ...  # prepended to /auth/*, /sessions, /chat/...
 */
/** Default: Makefile `make dev-start` API on http://localhost:8000 */
const base = process.env.ASTRA_SDK_BASE_URL || 'http://127.0.0.1:8000';
if (!process.env.ASTRA_SDK_BASE_URL) {
  // eslint-disable-next-line no-console
  console.info(`[sdk-online-smoke] ASTRA_SDK_BASE_URL unset, using default: ${base}`);
}

const root = base.replace(/\/$/, '');
const pfx = (process.env.ASTRA_SDK_PATH_PREFIX || '').replace(/\/$/, '');

function apiPath(path) {
  const p = path.startsWith('/') ? path : `/${path}`;
  return pfx ? `${root}${pfx}${p}` : `${root}${p}`;
}

async function main() {
  const healthRes = await fetch(`${root}/health`);
  if (!healthRes.ok) {
    console.error(`GET /health failed: ${healthRes.status}`);
    process.exit(1);
  }
  console.log('GET /health OK');

  let accessToken = process.env.ASTRA_SDK_ACCESS_TOKEN;

  if (accessToken) {
    // use token
  } else {
    const user = process.env.ASTRA_SDK_USERNAME;
    const pass = process.env.ASTRA_SDK_PASSWORD;
    if (user && pass) {
      const loginRes = await fetch(apiPath('/auth/login'), {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ username: user, password: pass }),
      });
      if (!loginRes.ok) {
        console.error(`POST /auth/login failed: ${loginRes.status}`);
        process.exit(1);
      }
      const body = await loginRes.json();
      if (!body.access_token) {
        console.error('login response missing access_token');
        process.exit(1);
      }
      accessToken = body.access_token;
    }
  }

  if (!accessToken) {
    console.log('(no ASTRA_SDK_ACCESS_TOKEN or USERNAME+PASSWORD — health only)');
    return;
  }

  const auth = { Authorization: `Bearer ${accessToken}` };

  const meRes = await fetch(apiPath('/auth/me'), { headers: auth });
  if (!meRes.ok) {
    console.error(`GET /auth/me failed: ${meRes.status}`);
    process.exit(1);
  }
  const me = await meRes.json();
  console.log('GET /auth/me OK', me.user_id ?? me);

  const sessionsRes = await fetch(apiPath('/sessions'), { headers: auth });
  if (!sessionsRes.ok) {
    console.error(`GET /sessions failed: ${sessionsRes.status}`);
    process.exit(1);
  }
  const sessionsBody = await sessionsRes.json();
  const n = Array.isArray(sessionsBody.sessions) ? sessionsBody.sessions.length : '?';
  console.log('GET /sessions OK, count=', n);

  const runId = process.env.ASTRA_SDK_TEST_RUN_ID;
  if (runId) {
    const enc = encodeURIComponent(runId);
    const st = await fetch(apiPath(`/chat/runs/${enc}`), { headers: auth });
    console.log(`GET /chat/runs/${runId} -> ${st.status}`);

    const ev = await fetch(apiPath(`/chat/runs/${enc}/stream?last_index=0`), { headers: auth });
    console.log(`GET /chat/runs/.../stream?last_index=0 -> ${ev.status} (${ev.headers.get('content-type') || 'no type'})`);
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
