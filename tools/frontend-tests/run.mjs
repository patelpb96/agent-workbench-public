// Pure execution modules + actual Svelte SSR markup; no browser/native app/provider/network.
import { mkdirSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const root = fileURLToPath(new URL('../../', import.meta.url));
const source = join(root, 'frontend', 'src', 'execution');
const output = join(root, 'artifacts', 'frontend-contract-src');
const compiler = join(root, 'frontend', 'node_modules', 'typescript', 'bin', 'tsc');
mkdirSync(output, { recursive: true });
const compiled = spawnSync(process.execPath, [compiler,
  '--strict', '--target', 'ES2022', '--module', 'commonjs',
  '--moduleResolution', 'node', '--skipLibCheck', '--noEmitOnError',
  // rootDir is frontend/src so the v2 generated contract (../execution-v2) is
  // emitted under the output tree, never beside its source file.
  '--rootDir', join(root, 'frontend', 'src'), '--outDir', output,
  ...['generated.ts', 'client.ts', 'reconcile.ts', 'workspace-ports.ts', 'controller.ts', 'layout.ts', 'module-checks.ts']
    .map(name => join(source, name)),
  join(root, 'frontend', 'src', 'execution-v2', 'generated.ts'),
  join(root, 'frontend', 'src', 'terminals', 'client.ts'),
  join(root, 'frontend', 'src', 'terminals', 'panes.ts'),
  join(root, 'frontend', 'src', 'terminals', 'keymap.ts'),
  join(root, 'frontend', 'src', 'terminals', 'copy-mode.ts'),
  join(root, 'frontend', 'src', 'terminals', 'spatial.ts'),
  join(root, 'frontend', 'src', 'terminals', 'search.ts'),
  join(root, 'frontend', 'src', 'terminals', 'hub.ts'),
  join(root, 'frontend', 'src', 'sidebars.ts'),
  join(root, 'frontend', 'src', 'terminals', 'floating.ts'),
  join(root, 'frontend', 'src', 'terminals', 'agents.ts'),
  join(root, 'frontend', 'src', 'terminals', 'models.ts'),
  join(root, 'frontend', 'src', 'settings', 'settings.ts'),
  join(root, 'frontend', 'src', 'settings', 'locations.ts'),
  join(root, 'frontend', 'src', 'files', 'files.ts'),
  join(root, 'frontend', 'src', 'files', 'follow.ts'),
  join(root, 'frontend', 'src', 'inspector', 'client.ts'),
  join(root, 'frontend', 'src', 'inspector', 'groups.ts'),
  join(root, 'frontend', 'src', 'inspector', 'map.ts'),
], { cwd: root, stdio: 'inherit' });
if (compiled.status !== 0) process.exit(compiled.status ?? 1);
// One TypeScript writer, then actual-module suites and separately bundled SSR markup.
const componentTests = join(root, 'tools', 'frontend-tests');
const suites = [
  ...readdirSync(source).filter(name => name.endsWith('.test.cjs')).sort().map(name => join(source, name)),
  ...readdirSync(componentTests).filter(name => name.endsWith('.test.mjs')).sort().map(name => join(componentTests, name)),
];
const tested = spawnSync(process.execPath, ['--test', '--test-reporter=tap', ...suites], {
  cwd: root, stdio: 'inherit',
});
process.exit(tested.status ?? 1);
