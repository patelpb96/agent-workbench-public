import { createRequire } from 'node:module';
import { NativeBridge, executionCommands } from './native-bridge.mjs';
import { TransportFaults } from './transport-faults.mjs';
const requireFrontend = createRequire(new URL('../../frontend/package.json', import.meta.url));
const { test: base, expect } = requireFrontend('@playwright/test');
export { expect };
// The shared fixture: a page wired to an owned native test process (real coordinator + SQLite).
export const test = base.extend({
  // The title bar slides away unless the cursor rests on the top edge; most tests click its buttons, so it is pinned (a test of the sliding itself clears this).
  page: async ({ page }, use) => {
    await page.addInitScript(() => { try { if (localStorage.getItem('aw.titlebar') === null) localStorage.setItem('aw.titlebar', 'pinned'); } catch { /* storage unavailable */ } });
    // Most tests drive the Sessions panel, so it is the open panel (a user's default is Files; the panel tests clear this).
    await page.addInitScript(() => { try { if (localStorage.getItem('aw.arrangement') === null) localStorage.setItem('aw.arrangement', JSON.stringify({ side: 'left', panel: 'sessions' })); } catch { /* storage unavailable */ } });
    await use(page);
  },
  nativeHistory: [false, { option: true }],
  transportFaults: async ({}, use) => { await use(new TransportFaults()); },
  bridge: async ({ page, transportFaults, nativeHistory }, use, testInfo) => {
    const bridge = new NativeBridge(process.env.HEADLESS_BRIDGE_EXE, nativeHistory);
    const pageErrors = [];
    page.on('pageerror', error => { if (pageErrors.length < 10) pageErrors.push(error.message); });
    let latest = null, sending = false, disposed = false;
    const deliver = () => {
      if (disposed || sending || latest === null) return;
      const payload = latest; latest = null; sending = true;
      void page.evaluate(value => globalThis.__headlessNotify?.(value), payload)
        .catch(() => {}) // Old page during reload; bootstrap/poll recovers authority.
        .finally(() => { sending = false; if (latest !== null) setTimeout(deliver, 0); });
    };
    try {
      await bridge.start();
      const origin = new URL(process.env.HEADLESS_BASE_URL).origin;
      // Fresh browser context/profile. Even accidental app requests cannot reach providers.
      // The sidebar sections that start collapsed are opened, as a user who has expanded them would have them.
      await page.addInitScript(() => { try { if (localStorage.getItem('aw.sections') === null) localStorage.setItem('aw.sections', JSON.stringify({ roster: true, runs: true, layout: true })); } catch { /* storage unavailable */ } });
      await page.route('**/*', route => new URL(route.request().url()).origin === origin ? route.continue() : route.abort());
      await page.exposeBinding('__headlessInvoke', async ({ frame }, call) => {
        if (new URL(frame.url()).origin !== origin || !call || !executionCommands.includes(call.command) ||
            !Object.hasOwn(call, 'request') || call.request === undefined) {
          throw new Error('Non-test origin/command refused.');
        }
        return transportFaults.forward(bridge, call);
      });
      bridge.onChanged = payload => { latest = payload; deliver(); }; // One in flight + one coalesced hint.
      await use(bridge);
      expect(pageErrors).toEqual([]);
    } finally {
      disposed = true; latest = null; bridge.onChanged = null;
      await page.close().catch(() => {});
      try { await bridge.cleanup(); }
      finally {
        await testInfo.attach('isolated-native-evidence', { contentType: 'application/json',
          body: Buffer.from(JSON.stringify({ ...bridge.evidence, commands: bridge.counts, page_errors: pageErrors,
            transport_fault: transportFaults.evidence }, null, 2)) });
      }
    }
  }
});
