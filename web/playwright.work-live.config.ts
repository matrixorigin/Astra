import { defineConfig, devices } from '@playwright/test';

// This config is intentionally separate from the mocked product fixture in
// playwright.config.ts. The live journey is opt-in and must never be picked up
// by `npm run test:e2e` or by CI's offline Web lane.
const webUrl = process.env.ASTRA_WORK_LIVE_WEB_URL ?? 'http://127.0.0.1:3537';
const outputDir = process.env.ASTRA_WORK_LIVE_OUTPUT_DIR ?? './test-results-work-live';

export default defineConfig({
  testDir: './e2e-live',
  outputDir,
  timeout: 120_000,
  expect: { timeout: 20_000 },
  fullyParallel: false,
  workers: 1,
  forbidOnly: !!process.env.CI,
  retries: 0,
  reporter: process.env.CI ? [['line']] : 'line',
  use: {
    baseURL: webUrl,
    // Public runtime-config enrollment sets the browser session selector. Traces can
    // retain request headers/cookies, so keep them disabled; screenshots and
    // videos remain useful without copying credentials into evidence.
    trace: 'off',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
  },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
