// Non-interfering, one-shot runner: no desktop app, user browser or OS input.
import { createRequire } from 'node:module';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { dirname, join, resolve } from 'node:path';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { spawn } from 'node:child_process';

const root = fileURLToPath(new URL('../../', import.meta.url));
const frontend = join(root, 'frontend');
const requireFrontend = createRequire(join(frontend, 'package.json'));
const output = join(root, 'artifacts/headless-tests');
if (process.argv.length !== 2 || (process.env.PWDEBUG && process.env.PWDEBUG !== '0')) {
  throw new Error('This runner accepts no headed/debug/browser arguments.');
}
await mkdir(output, { recursive: true });
async function run(command, args, env = process.env, collect = false) {
  const child = spawn(command, args, { cwd: root, env, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
  let stdout = '', stderr = '', overflow = false;
  child.stdout.setEncoding('utf8'); child.stderr.setEncoding('utf8');
  child.stdout.on('data', chunk => {
    if (collect) {
      if (overflow) return;
      if (stdout.length + chunk.length > 20 * 1024 * 1024) { overflow = true; child.kill(); return; }
      stdout += chunk;
    } else process.stdout.write(chunk);
  });
  child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-65536); process.stderr.write(chunk); });
  return new Promise((resolveRun, reject) => {
    child.on('error', reject);
    child.on('close', code => code === 0 && !overflow ? resolveRun(stdout) : reject(new Error(`Headless prerequisite failed (${overflow ? 'output budget' : code}): ${stderr}`)));
  });
}
await run(process.execPath, ['--test', join(root, 'tools/headless-tests/transport-faults.test.mjs')]);
// Serialize writers. Cargo's test artifact is distinct from the release app.
const cargoArgs = ['test', '--locked', '--target', 'x86_64-pc-windows-msvc', '--test', 'headless_bridge', '--no-run', '--message-format=json'];
const compiled = process.platform === 'win32'
  ? await run('cmd.exe', ['/d', '/c', 'src-tauri\\check-vs2022.cmd', ...cargoArgs], process.env, true)
  : await run('cargo', ['test', '--manifest-path', 'src-tauri/Cargo.toml', '--locked', '--test', 'headless_bridge', '--no-run', '--message-format=json'], process.env, true);
await writeFile(join(output, 'native-build.jsonl'), compiled, 'utf8');
const artifacts = compiled.split('\n').flatMap(line => { try { return [JSON.parse(line)]; } catch { return []; } });
const binary = artifacts.find(value => value.reason === 'compiler-artifact' && value.target?.name === 'headless_bridge' && value.executable)?.executable;
if (!binary || !resolve(binary).startsWith(resolve(root, 'src-tauri/target') + (process.platform === 'win32' ? '\\' : '/'))) {
  throw new Error('Cargo did not produce the expected isolated integration-test artifact.');
}
const { createServer } = await import(pathToFileURL(requireFrontend.resolve('vite')).href);
const { svelte } = await import(pathToFileURL(requireFrontend.resolve('@sveltejs/vite-plugin-svelte')).href);
const site = join(root, 'tools/headless-tests/site');
let html = await readFile(join(site, 'index.html'), 'utf8');
html = html.replace('src="/main.js"', `src="/@fs/${join(site, 'main.js').replaceAll('\\', '/')}"`);
const server = await createServer({
  configFile: false, root: frontend, envDir: false, envPrefix: '__HEADLESS_NO_PUBLIC_ENV__',
  clearScreen: false, logLevel: 'warn',
  plugins: [{ name: 'isolated-headless-entry', configureServer(dev) {
    dev.middlewares.use(async (request, response, next) => {
      if (request.url !== '/' && request.url !== '/index.html') return next();
      try {
        response.setHeader('Content-Type', 'text/html; charset=utf-8');
        response.end(await dev.transformIndexHtml('/', html));
      } catch (error) { next(error); }
    });
  } }, svelte()],
  server: { host: '127.0.0.1', port: 0, hmr: false, fs: { allow: [frontend, dirname(site)] } }
});
try {
  await server.listen();
  const baseURL = server.resolvedUrls.local[0];
  console.log(`Headless UI test server: ${baseURL} (loopback only)`);
  await run(process.execPath, [requireFrontend.resolve('@playwright/test/cli'), 'test', '--config', join(root, 'tools/headless-tests/playwright.config.mjs')], {
    ...process.env, HEADLESS_BRIDGE_EXE: binary, HEADLESS_BASE_URL: baseURL,
    PLAYWRIGHT_BROWSERS_PATH: join(root, 'artifacts/playwright-browsers'),
    PWDEBUG: '0'
  });
} finally { await server.close(); }
