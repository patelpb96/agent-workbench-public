import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
const requireFrontend = createRequire(new URL('../../frontend/package.json', import.meta.url));
const { defineConfig } = requireFrontend('@playwright/test');
if (!process.env.HEADLESS_BASE_URL || !process.env.HEADLESS_BRIDGE_EXE) throw new Error('Use the isolated headless runner.');
const origin = new URL(process.env.HEADLESS_BASE_URL);
if (origin.protocol !== 'http:' || origin.hostname !== '127.0.0.1' || !origin.port || origin.username || origin.password || origin.pathname !== '/') {
  throw new Error('Only the isolated loopback test origin is allowed.');
}
export default defineConfig({
  testDir: fileURLToPath(new URL('.', import.meta.url)), testMatch: '*.spec.mjs',
  workers: 1, fullyParallel: false, forbidOnly: true, retries: 0,
  timeout: 60_000, expect: { timeout: 10_000 },
  reporter: [['list'], ['json', { outputFile: fileURLToPath(new URL('../../artifacts/headless-tests/results.json', import.meta.url)) }]],
  outputDir: fileURLToPath(new URL('../../artifacts/headless-tests/test-output', import.meta.url)),
  use: { browserName: 'chromium', headless: true, baseURL: process.env.HEADLESS_BASE_URL,
    viewport: { width: 1440, height: 960 }, actionTimeout: 10_000,
    trace: 'retain-on-failure', screenshot: 'only-on-failure', video: 'off' }
});
