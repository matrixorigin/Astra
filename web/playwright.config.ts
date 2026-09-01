import { defineConfig, devices } from '@playwright/test';

// Playwright probes and drives a loopback-only server. Developer machines may
// define an HTTP proxy without a matching bypass, which would turn readiness
// checks into remote 503s and hide the actual browser result behind a timeout.
const proxyBypass = new Set(
  [process.env.NO_PROXY, process.env.no_proxy]
    .flatMap((value) => value?.split(',') ?? [])
    .map((value) => value.trim())
    .filter(Boolean),
);
proxyBypass.add('127.0.0.1');
proxyBypass.add('localhost');
process.env.NO_PROXY = [...proxyBypass].join(',');
process.env.no_proxy = process.env.NO_PROXY;

export default defineConfig({
  testDir: './e2e',
  timeout: 30_000,
  expect: {
    timeout: 5_000,
  },
  use: {
    baseURL: 'http://127.0.0.1:3536',
    trace: 'on-first-retry',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
    {
      name: 'mobile-chromium',
      use: {
        ...devices['Desktop Chrome'],
        viewport: { width: 375, height: 812 },
        isMobile: true,
        hasTouch: true,
      },
    },
  ],
  webServer: {
    command: 'npm run dev',
    url: 'http://127.0.0.1:3536/e2e/chat-view',
    reuseExistingServer: !process.env.CI,
    env: {
      ASTRA_ENABLE_E2E_PAGES: '1',
      ASTRA_WEB_HOST: '127.0.0.1',
      ASTRA_WEB_PORT: '3536',
    },
    timeout: 120_000,
  },
});
