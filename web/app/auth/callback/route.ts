import { NextRequest } from 'next/server';

// This callback receives only an OAuth code. Its authenticated opener owns
// completion; localhost and 127.0.0.1 need not share an Astra session cookie.
export function GET(request: NextRequest) {
  const url = new URL(request.url);
  const fields = ['state', 'code', 'client_id', 'error'];
  const values: Record<string, string> = {};
  for (const field of fields) {
    const entries = url.searchParams.getAll(field);
    if (entries.length > 1 || entries.some((value) => value.length > 16384)) return new Response('Invalid authorization callback', { status: 400, headers: { 'Cache-Control': 'no-store', 'Referrer-Policy': 'no-referrer' } });
    if (entries.length === 1) values[field] = entries[0];
  }
  if (!values.state || Boolean(values.code) === Boolean(values.error)) return new Response('Invalid authorization callback', { status: 400, headers: { 'Cache-Control': 'no-store', 'Referrer-Policy': 'no-referrer' } });
  const nonce = crypto.randomUUID();
  const data = JSON.stringify({ type: 'astra-chatgpt-callback', ...values }).replace(/</g, '\\u003c');
  return new Response(`<!doctype html><html><head><meta name="referrer" content="no-referrer"><title>Astra authorization</title></head><body><p>Authorization received. Return to Astra.</p><script nonce="${nonce}">history.replaceState(null,'','/auth/callback');if(window.opener){const port=location.port?':'+location.port:'';for(const origin of ['http://127.0.0.1'+port,'http://localhost'+port])window.opener.postMessage(${data},origin);}</script></body></html>`, {
    headers: { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store', 'Referrer-Policy': 'no-referrer',
      'Content-Security-Policy': `default-src 'none'; script-src 'nonce-${nonce}'; frame-ancestors 'none'; base-uri 'none'` },
  });
}
