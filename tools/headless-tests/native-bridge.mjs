// Only an owned Rust integration-test executable is spawned; never the desktop app.
import { spawn } from 'node:child_process';
import { readFile, lstat, realpath, rm } from 'node:fs/promises';
import { dirname, basename, join, relative, isAbsolute, sep } from 'node:path';
import { fileURLToPath } from 'node:url';
import { tmpdir } from 'node:os';

import { executionCommands } from './commands.mjs';
export { executionCommands } from './commands.mjs';
const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const MAX_LINE = 8 * 1024 * 1024;
const waitFor = (promise, ms, label) => {
  let timer;
  return Promise.race([promise, new Promise((_, reject) => { timer = setTimeout(() => reject(new Error(label)), ms); })])
    .finally(() => clearTimeout(timer));
};

export class NativeBridge {
  #executable; #history; #child; #lease; #owned = false; #pending = new Map(); #nextId = 0;
  #ready; #resolveReady; #rejectReady; #exit; #stopping = false; #requestLimit = 0; #exitConfirmed = false;
  onChanged = null;
  evidence = { native_starts: 0, normal_exits: 0, owned_workspace_removed: false };
  trace = [];
  counts = Object.fromEntries([...executionCommands].map(command => [command.replace(/_v2$/, ''), 0]));
  constructor(executable, history = false) {
    if (typeof history !== 'boolean') throw new Error('Only a boolean native history fixture selector is allowed.');
    this.#history = history;
    this.evidence.history_fixture = history;
    if (!executable || !/^headless_bridge-[\da-f]+(?:\.exe)?$/.test(basename(executable))) {
      throw new Error('Only a compiled headless_bridge integration test is allowed.');
    }
    this.#executable = executable;
  }
  async #validateLease(lease) {
    if (!lease || Object.keys(lease).sort().join(',') !== 'nonce,root' || !uuid.test(lease.nonce) || typeof lease.root !== 'string') {
      throw new Error('Invalid native test lease.');
    }
    const info = await lstat(lease.root);
    const root = await realpath(lease.root);
    if (!info.isDirectory() || info.isSymbolicLink() || dirname(root) !== await realpath(tmpdir()) ||
        !uuid.test(basename(root).replace(/^agent-workbench-headless-/, '')) ||
        !basename(root).startsWith('agent-workbench-headless-')) throw new Error('Non-isolated test root refused.');
    const marker = join(root, 'headless-owner.json');
    const markerInfo = await lstat(marker);
    if (!markerInfo.isFile() || markerInfo.isSymbolicLink() || markerInfo.size > 4096) throw new Error('Invalid test owner marker.');
    const stored = JSON.parse(await readFile(marker, 'utf8'));
    if (stored.nonce !== lease.nonce || await realpath(stored.root) !== root) throw new Error('Test lease mismatch.');
    return root;
  }
  #fail(error) {
    this.#rejectReady?.(error);
    for (const pending of this.#pending.values()) { clearTimeout(pending.timer); pending.reject(error); }
    this.#pending.clear();
  }
  async start() {
    if (this.#child) throw new Error('Bridge already started.');
    const artifact = await realpath(this.#executable);
    const target = await realpath(fileURLToPath(new URL('../../src-tauri/target/', import.meta.url)));
    const within = relative(target, artifact);
    const info = await lstat(this.#executable);
    if (!info.isFile() || info.isSymbolicLink() || isAbsolute(within) || within === '..' || within.startsWith('..' + sep)) {
      throw new Error('Bridge must be a regular compiled artifact inside this project target.');
    }
    this.#stopping = false;
    this.#exitConfirmed = false;
    this.#ready = new Promise((resolve, reject) => { this.#resolveReady = resolve; this.#rejectReady = reject; });
    const env = { ...process.env, WORKBENCH_HEADLESS_BRIDGE: '1' };
    // Never inherit a root from another invocation or expose this lease to the page.
    delete env.WORKBENCH_HEADLESS_ROOT; delete env.WORKBENCH_HEADLESS_NONCE;
    delete env.WORKBENCH_HEADLESS_SEED_HISTORY;
    if (this.#history && !this.#lease) env.WORKBENCH_HEADLESS_SEED_HISTORY = '1';
    if (this.#lease) { env.WORKBENCH_HEADLESS_ROOT = this.#lease.root; env.WORKBENCH_HEADLESS_NONCE = this.#lease.nonce; }
    const child = spawn(this.#executable, ['--ignored', '--exact', 'serve_headless_bridge', '--nocapture', '--test-threads=1'],
      { env, stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true });
    this.#child = child;
    let buffered = '', stderr = '';
    child.stdin.on('error', error => this.#fail(error));
    child.stderr.setEncoding('utf8');
    child.stderr.on('data', chunk => { stderr = (stderr + chunk).slice(-4096); });
    child.stdout.setEncoding('utf8');
    child.stdout.on('data', chunk => {
      buffered += chunk;
      if (buffered.length > MAX_LINE) { this.#fail(new Error('Test bridge output exceeded budget.')); child.stdin.destroy(); return; }
      let end;
      while ((end = buffered.indexOf('\n')) !== -1) {
        const line = buffered.slice(0, end).replace(/\r$/, ''); buffered = buffered.slice(end + 1);
        const offset = line.indexOf('AW_HEADLESS\t');
        if (offset < 0) continue; // Rust test-runner preamble only.
        try {
          if (offset !== 0 && line.slice(0, offset) !== 'test serve_headless_bridge ... ') throw new Error('Invalid native frame prefix.');
          const frame = JSON.parse(line.slice(offset + 12));
          if (frame.type === 'ready') this.#resolveReady(frame);
          else if (frame.type === 'changed') this.onChanged?.(frame.payload);
          else if (frame.type === 'reply' && this.#pending.has(frame.id)) {
            const pending = this.#pending.get(frame.id); this.#pending.delete(frame.id); clearTimeout(pending.timer);
            if (('ok' in frame) === ('error' in frame)) pending.reject(new Error('Invalid native reply.'));
            else pending.resolve('ok' in frame ? { ok: frame.ok } : { error: frame.error });
          } else throw new Error('Unexpected native frame.');
        } catch (error) { this.#fail(error); child.stdin.destroy(); }
      }
    });
    child.on('error', error => this.#fail(error));
    this.#exit = new Promise(resolve => child.once('close', (code, signal) => {
      this.#exitConfirmed = true;
      this.#fail(new Error(`Owned headless bridge exited (${code ?? signal}).`));
      resolve({ code, signal, stderr });
    }));
    try {
      const ready = await waitFor(this.#ready, 15_000, 'Native headless bridge startup timed out.');
      const { lease } = ready;
      if (!Number.isSafeInteger(ready.request_limit) || ready.request_limit < 1024 || ready.request_limit > 1024 * 1024) throw new Error('Invalid native byte budget.');
      this.#requestLimit = ready.request_limit;
      await this.#validateLease(lease);
      if (this.#lease && (lease.root !== this.#lease.root || lease.nonce !== this.#lease.nonce)) throw new Error('Restart changed lease.');
      if (!this.#lease) { this.#lease = Object.freeze({ ...lease }); this.#owned = true; }
      this.evidence.native_starts++;
    } catch (error) { await this.stop().catch(() => {}); throw error; }
  }
  async call(name, request) {
    if (request === undefined) throw new Error('Headless calls require an explicit request.');
    // Native speaks v2 command names; trusted Node helpers may use logical names.
    const command = typeof name === 'string' && !name.endsWith('_v2') ? name + '_v2' : name;
    const logical = typeof command === 'string' ? command.replace(/_v2$/, '') : command;
    if (!this.#child || this.#stopping || !executionCommands.includes(command) || this.#pending.size >= 32) {
      throw new Error('Headless execution transport unavailable or command refused.');
    }
    const id = ++this.#nextId;
    if (!Number.isSafeInteger(id)) throw new Error('Headless request counter exhausted.');
    const body = JSON.stringify({ id, command, request }) + '\n';
    if (Buffer.byteLength(body) > this.#requestLimit) throw new Error('Headless request exceeds its byte budget.');
    this.counts[logical] = (this.counts[logical] ?? 0) + 1;
    this.trace.push({ command: logical, request_id: request?.request_id ?? null });
    if (this.trace.length > 256) this.trace.shift();
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => { this.#pending.delete(id); reject(new Error('Headless native reply timed out.')); }, 12_000);
      this.#pending.set(id, { resolve, reject, timer });
      this.#child.stdin.write(body, error => {
        if (error && this.#pending.has(id)) { this.#pending.delete(id); clearTimeout(timer); reject(error); }
      });
    });
  }
  async invoke(command, request) {
    const result = await this.call(command, request);
    if ('error' in result) throw result.error;
    return result.ok;
  }
  async snapshot() { return (await this.invoke('bootstrap_workspace', { contract_version: 2 })).snapshot; }
  async stop() {
    if (!this.#child) return;
    const child = this.#child; this.#stopping = true;
    child.stdin.end(); // Orderly native Coordinator::shutdown, no fake terminal invented.
    let exit;
    try { exit = await waitFor(this.#exit, 15_000, 'Owned headless bridge did not close.'); }
    catch (error) {
      // Only this invocation's integration-test child, never agent-workbench.exe.
      child.kill();
      await waitFor(this.#exit, 5000, 'Owned test child exit could not be confirmed.');
      throw error;
    } finally { if (this.#exitConfirmed) this.#child = null; }
    if (exit.code !== 0) throw new Error(`Headless bridge failed: ${exit.stderr || exit.code}`);
    this.evidence.normal_exits++;
  }
  // Fault injection available only to trusted Node tests, never the page binding.
  async crashOwnedForTest() {
    if (!this.#child || this.#stopping) throw new Error('No owned test child to crash.');
    this.#child.kill();
    await waitFor(this.#exit, 5000, 'Owned fault-injection child did not exit.');
  }
  async restart() { await this.stop(); await this.start(); }
  async cleanup() {
    this.onChanged = null;
    let stopFailure;
    try { await this.stop(); } catch (error) { stopFailure = error; }
    try {
      // Never remove a root while child termination is unconfirmed. A crash
      // still releases handles, so its verified owned lease can be removed.
      if (!this.#child && this.#owned && this.#lease) {
        const root = await this.#validateLease(this.#lease);
        await rm(root, { recursive: true }); // Exact canonical owned lease, never a sweep.
        this.#owned = false;
        this.evidence.owned_workspace_removed = true;
      }
    } catch (error) {
      if (stopFailure) throw new AggregateError([stopFailure, error], 'Owned test stop and cleanup failed.');
      throw error;
    }
    if (stopFailure) throw stopFailure;
  }
}
