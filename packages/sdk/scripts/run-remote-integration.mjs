#!/usr/bin/env node
/**
 * Run Vitest Mode B (remote) with defaults for local dev: E2E=1 and
 * ASTRA_SDK_BASE_URL=http://127.0.0.1:6789 when unset (matches Makefile dev-start).
 *
 *   npm run test:integration:remote
 *   ASTRA_SDK_BASE_URL=http://other:9 npm run test:integration:remote
 *   ASTRA_SDK_ACCESS_TOKEN=... npm run test:integration:remote
 */
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const pkgRoot = join(here, '..');
const vitestBin = join(pkgRoot, 'node_modules', 'vitest', 'vitest.mjs');

process.env.ASTRA_SDK_E2E = '1';
if (!process.env.ASTRA_SDK_BASE_URL) {
  process.env.ASTRA_SDK_BASE_URL = 'http://127.0.0.1:6789';
}

const code =
  spawnSync(
    process.execPath,
    [vitestBin, 'run', 'src/__tests__/integration/online'],
    { stdio: 'inherit', env: process.env, cwd: pkgRoot },
  ).status ?? 1;
process.exit(code === 0 ? 0 : code);
