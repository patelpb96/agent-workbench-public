// Project-local browser download only; no system browser/profile/settings changes.
import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
const root = fileURLToPath(new URL('../../', import.meta.url));
const requireFrontend = createRequire(join(root, 'frontend/package.json'));
const child = spawn(process.execPath, [requireFrontend.resolve('@playwright/test/cli'), 'install', 'chromium'], {
  cwd: root, env: { ...process.env, PLAYWRIGHT_BROWSERS_PATH: join(root, 'artifacts/playwright-browsers') },
  windowsHide: true, stdio: ['ignore', 'inherit', 'inherit']
});
child.on('error', error => { console.error(error.message); process.exitCode = 1; });
child.on('close', code => { process.exitCode = code ?? 1; });
