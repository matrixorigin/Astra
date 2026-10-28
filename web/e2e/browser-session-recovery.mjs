// Driven by browser_session_next_db_it against real production Next/runtime/DB.
// The fixture arrives on stdin so authentication proofs never enter argv/logs.
import assert from 'node:assert/strict';
import { chromium, expect } from '@playwright/test';

let input = '';
for await (const chunk of process.stdin) input += chunk;
const fixture = JSON.parse(input);
const browser = await chromium.launch();
try {
  const context = await browser.newContext();
  const separator = fixture.cookie.indexOf('=');
  const captured = fixture.cookie.slice(separator + 1);
  const offline = fixture.mode.startsWith('offline-');
  const original = offline
    ? `bs3.${Buffer.from('http://127.0.0.1:1/').toString('base64url')}.${captured.split('.')[2]}.${captured.split('.')[3]}.${captured.split('.')[4]}`
    : captured;
  await context.addCookies([{
    name: fixture.cookie.slice(0, separator), value: original, url: fixture.urls[0],
    httpOnly: true, secure: true, sameSite: 'Lax',
  }]);
  const page = await context.newPage();
  const selected = async () => (await context.cookies(fixture.urls[0]))
    .find(cookie => cookie.name === 'astra_session')?.value;
  const response = await page.goto(fixture.urls[0] + fixture.path);
  await expect(page.getByRole('heading', { name: 'Unable to load this page' })).toBeVisible();
  await expect(page.locator('nav[aria-label="Mobile primary navigation"]')).toHaveCount(0);
  const loginPath = `/login?${new URLSearchParams({ next: fixture.path })}`;
  await expect(page.getByRole('link', { name: 'Sign in', exact: true })).toHaveAttribute('href', loginPath);
  assert(!(response.headers()['set-cookie'] ?? '').includes('astra_session='), 'render must not replace authentication');
  assert(await selected() === original, 'error rendering must retain the captured proof');
  assert.equal((await context.request.get(fixture.urls[0] + '/api/runtime-auth/me')).status(), 503);

  if (fixture.mode === 'retry') {
    // Another explicit request recovers the session while this page retains its
    // failed RSC tree. reset() alone reuses that failure; retry must fetch anew.
    const login = await context.request.post(fixture.urls[0] + '/api/runtime-auth/login', {
      data: { apiUrl: fixture.api_url, username: fixture.username, password: fixture.password },
      maxRedirects: 0,
    });
    assert.equal(login.status(), 200, 'explicit login must replace the unusable owner');
    await Promise.all([
      page.waitForNavigation({ waitUntil: 'domcontentloaded' }),
      page.getByRole('link', { name: 'Try again' }).click(),
    ]);
    await expect(page).toHaveURL(fixture.urls[0] + fixture.path);
  } else {
    await page.getByRole('link', { name: 'Sign in', exact: true }).click();
    await expect(page).toHaveURL(fixture.urls[0] + loginPath);
    await page.getByRole('button', { name: 'Sign out previous browser session' }).click();
    if (offline) {
      await expect(page.getByText('Could not sign out the previous session. Please retry.', { exact: true })).toBeVisible();
      if (fixture.mode === 'offline-explicit') {
        await page.getByLabel(/runtime URL/i).fill(fixture.api_url);
      } else {
        await page.getByRole('button', { name: 'Select runtime' }).click();
        await expect(page.getByRole('status')).toContainText('Runtime selected');
      }
    } else {
      await expect(page.getByRole('status')).toContainText('Previous browser session signed out');
      assert.equal((await context.request.get(fixture.urls[0] + '/api/runtime-auth/me')).status(), 401);
    }
    await page.getByLabel('Username', { exact: true }).fill(fixture.username);
    await page.getByLabel('Password', { exact: true }).fill(fixture.password);
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    await expect(page).toHaveURL(fixture.urls[0] + fixture.path);
  }
  await expect(page.getByRole('heading', { name: 'Unable to load this page' })).toHaveCount(0);
  await expect(page.locator('nav[aria-label="Mobile primary navigation"]')).toHaveCount(1);
  if (fixture.mode === 'sign-in') {
    await page.goto(fixture.urls[0] + '/login?' + new URLSearchParams({ next: '/a/..//evil.example' }));
    await page.getByLabel('Username', { exact: true }).fill(fixture.username);
    await page.getByLabel('Password', { exact: true }).fill(fixture.password);
    await page.getByRole('button', { name: 'Sign in', exact: true }).click();
    // /overview is the safe fallback and currently redirects to the home page.
    await expect(page).toHaveURL(fixture.urls[0] + '/');
  }
  assert(await selected() !== original, 'only explicit login replaces the broken owner');
  for (const url of fixture.urls) {
    const me = await context.request.get(url + '/api/runtime-auth/me');
    assert.equal(me.status(), 200, 'both Web replicas must recognize the recovered session');
    assert.equal((await me.json()).user_id, fixture.user_id);
  }
} finally {
  await browser.close();
}
