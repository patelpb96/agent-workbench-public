#!/usr/bin/env node
// Actual Svelte UI + the existing in-page terminal/observer facade. All sessions
// are synthetic. No desktop capture, OS input, native broker or provider calls.
import { createRequire } from 'node:module';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { resolve, join, dirname } from 'node:path';
import { pathToFileURL, fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
const { values } = parseArgs({ options: { source: { type: 'string' }, out: { type: 'string' } }, strict: true });
const root = resolve(values.source ?? fileURLToPath(new URL('../..', import.meta.url)));
if (!values.out || process.env.PWDEBUG && process.env.PWDEBUG !== '0') throw new Error('Supply a new private --out directory; headless only.');
const out = resolve(values.out);
if (out === root || out.startsWith(root + '/')) throw new Error('Keep raw demo frames outside source.');
await mkdir(out, { recursive: false });
const frontend = join(root, 'frontend');
const req = createRequire(join(frontend, 'package.json'));
process.env.PLAYWRIGHT_BROWSERS_PATH = join(root, 'artifacts/playwright-browsers');
const { chromium } = req('@playwright/test');
const { createServer } = await import(pathToFileURL(req.resolve('vite')).href);
const { svelte } = await import(pathToFileURL(req.resolve('@sveltejs/vite-plugin-svelte')).href);
const site = join(root, 'tools/headless-tests/site');
let html = await readFile(join(site, 'index.html'), 'utf8');
html = html.replace('src="/main.js"', `src="/@fs/${join(site, 'main.js').replaceAll('\\', '/')}"`);
const server = await createServer({
  configFile: false, root: frontend, envDir: false, envPrefix: '__NO_DEMO_SECRETS__', logLevel: 'error',
  plugins: [{ name: 'public-synthetic-demo', configureServer(dev) {
    dev.middlewares.use(async (request, response, next) => {
      if (!['/', '/index.html'].includes(request.url)) return next();
      try { response.setHeader('Content-Type', 'text/html; charset=utf-8'); response.end(await dev.transformIndexHtml('/', html)); }
      catch (error) { next(error); }
    });
  } }, svelte()],
  server: { host: '127.0.0.1', port: 0, hmr: false, fs: { allow: [frontend, dirname(site)] } }
});
let browser, page;
const errors = [];
try {
  await server.listen();
  const origin = new URL(server.resolvedUrls.local[0]).origin;
  browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({ viewport: { width: 1680, height: 1050 }, locale: 'en-US', timezoneId: 'UTC', serviceWorkers: 'block', reducedMotion: 'reduce' });
  page = await context.newPage();
  page.on('pageerror', error => errors.push(error.message));
  page.on('console', message => { if (message.type() === 'error') errors.push(message.text()); });
  await page.route('**/*', route => new URL(route.request().url()).origin === origin ? route.continue() : route.abort());
  await page.addInitScript(() => {
    localStorage.setItem('aw.titlebar', 'pinned');
    localStorage.setItem('aw.arrangement', JSON.stringify({ side: 'left', panel: null }));
    globalThis.__awRuntimeVersion = 'illustrative demo';
    const id = 'd0000000-0000-4000-8000-000000000001';
    const snapshot = { contract_version:2, workspace:{workspace_id:id,label:'Synthetic demo',created_at:'2000-01-01T00:00:00Z'},
      sessions:[],runs:[],layout:{layout_version:1,workspace_id:id,revision:0,theme:'dark',active_module:'agents',sidebar_width:300,panels:[],containers:[]},
      layout_warning:null,execution_available:true,codex_availability:'not_configured' };
    globalThis.__headlessInvoke = async call => {
      if (call.command === 'bootstrap_workspace_v2') return {ok:{contract_version:2,storage_state:'ready',upgrade_id:null,snapshot}};
      if (call.command === 'get_workspace_snapshot_v2') return {ok:snapshot};
      if (call.command === 'get_run_progress_v2') return {ok:{contract_version:2,workspace_id:id,run_id:null,steps:[]}};
      return {error:{code:'demo_read_only',user_message:'Illustrative demo; no execution backend is connected.',recoverable:true}};
    };
  });
  await page.goto(origin);
  await page.waitForFunction(() => globalThis.__fakeTerminals);
  const seed = () => {
    const fake = globalThis.__fakeTerminals;
    const labels = ['Pi · API refactor', 'Codex · test suite', 'Claude Code · review', 'Shell · build'];
    fake.sessions = labels.map((label, i) => ({ id: `d000000${i}`, label, state: 'running', persistent: true, cols: 96, rows: 30, host_pid: 1 }));
    fake.agents = ['pi', 'codex', 'claude'].map((agent, i) => ({ terminal: `d000000${i}`, agent, cwd: 'C:\\workspace\\demo', launched_ms: Date.now() }));
    fake.linked = ['d0000000','d0000001','d0000002'];
    fake.stream = {
      d0000000: '\u001b[36mPi · implementation\u001b[0m\r\n\r\nWorking in /workspace/demo\r\n> Refactor the API boundary\r\n\r\n✓ Read interface contracts\r\n✓ Added typed request validation\r\n✓ Updated focused regression tests\r\n\r\nNext: independent review\r\n',
      d0000001: '\u001b[32mCodex · verification\u001b[0m\r\n\r\n$ npm test\r\n\r\nPASS  request validation\r\nPASS  cancellation remains isolated\r\nPASS  restart preserves history\r\n\r\nTest suites: 3 passed (demo)\r\nNo live provider request\r\n',
      d0000002: '\u001b[35mClaude Code · review\u001b[0m\r\n\r\nReviewing the exact candidate diff\r\n\r\n✓ Public interfaces preserved\r\n✓ No unrelated files changed\r\n✓ Privacy scan queued\r\n\r\nWaiting for a human decision\r\n',
      d0000003: '\u001b[33mBuild terminal\u001b[0m\r\n\r\n$ npm run build\r\n\r\nvite · production bundle\r\nSvelte + TypeScript → Tauri\r\n\r\n✓ Ready for local smoke test (demo)\r\n\r\nSynthetic session data only\r\n'
    };
    fake.mapPayload = { generated_ms: Date.now(), reports: [], sessions: [
      { id: '0123456789abcdef', open_calls: 1, open_tools: ['Bash'], tools: [{name:'Bash',calls:10,errors:0},{name:'Read',calls:8,errors:0}] },
      { id: 'fedcba9876543210', open_calls: 0, open_tools: [], tools: [{name:'Read',calls:6,errors:0},{name:'Grep',calls:4,errors:0}] },
      { id: '1111111111111111', open_calls: 0, open_tools: [], tools: [{name:'Grep',calls:9,errors:0}] }
    ] };
  };
  await page.evaluate(seed);
  await page.reload();
  await page.evaluate(seed);
  const rail = name => page.locator('nav.rail').getByTitle(name, {exact:true});
  await rail('Terminals').click();
  await page.getByRole('button', { name: /^Tile all \(4\)/ }).click();
  await page.waitForTimeout(1300);
  await page.screenshot({path:join(out,'terminals.png')});
  const crop = page.locator('.panes .pane').first();
  await crop.screenshot({path:join(out,'session-detail.png')});
  const model = crop.getByRole('button', {name:'Model',exact:true});
  if (await model.count()) {
    await model.click();
    await page.waitForTimeout(150);
    await crop.screenshot({path:join(out,'model-picker.png')});
    await model.click();
  }
  for (let i=0;i<4;i++) await page.locator('.terminal-list li:not(.empty)').nth(i).locator('button.float-out').click();
  await rail('Canvas').click();
  await page.waitForTimeout(400);
  const area = await page.locator('.float-layer').boundingBox();
  for (const i of [3,2,1,0]) {
    const grip = await page.locator('.float-win .float-grip').nth(i).boundingBox();
    const x = area.x+24+(i%2)*610, y = area.y+24+Math.floor(i/2)*370;
    await page.mouse.move(grip.x+40,grip.y+10);
    await page.mouse.down(); await page.mouse.move(x+40,y+10,{steps:5}); await page.mouse.up();
  }
  await page.waitForTimeout(300);
  await page.screenshot({path:join(out,'canvas.png')});
  await rail('Inspector').click();
  await page.getByRole('tab', {name:'Map',exact:true}).click();
  await page.waitForTimeout(700);
  await page.screenshot({path:join(out,'inspector-map.png')});
  await page.locator('.ins-map').screenshot({path:join(out,'map-detail.png')});
  await writeFile(join(out,'visible-text-private.txt'),await page.locator('body').innerText(),'utf8');
  await writeFile(join(out,'capture.json'),JSON.stringify({kind:'actual UI with synthetic in-page backend',desktopCapture:false,
    captures:['terminals.png','canvas.png','inspector-map.png'],crops:['session-detail.png','model-picker.png','map-detail.png']},null,2));
  console.log('Saved synthetic demo frames and feature crops. No desktop capture.');
} catch (error) {
  if (page) {
    await page.screenshot({path:join(out,'failed-private.png')}).catch(()=>{});
    await writeFile(join(out,'failed-private.txt'), JSON.stringify({errors,body:await page.locator('body').innerText().catch(()=>''),message:error.message},null,2));
  }
  throw error;
} finally {
  if (browser) await browser.close();
  await server.close();
}
