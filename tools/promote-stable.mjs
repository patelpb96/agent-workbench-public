#!/usr/bin/env node
// Keep a known-good, frozen copy of Agent Workbench to work in, separate from the build you are changing.
//
//   node tools/promote-stable.mjs              copy the current release build to the stable folder
//   node tools/promote-stable.mjs --rollback   put the previous stable copy back
//
// The stable folder is %LOCALAPPDATA%\AgentWorkbench\stable. It holds the app, the terminal service executables
// (the app starts the service from its own folder), a VERSION.txt saying which commit and checksums it is, and a
// shortcut. Rebuilding with build-release.mjs never touches it. Run one app at a time: the run history database
// allows a single app, so close the stable app before starting a development build and the other way round.
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, readFileSync, readdirSync, renameSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const EXECUTABLES = ['agent-workbench.exe', 'wb-daemon.exe', 'wb-host.exe', 'wb.exe'];
const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
export const stableDir = () => join(process.env.LOCALAPPDATA ?? join(homedir(), 'AppData', 'Local'), 'AgentWorkbench', 'stable');

const sha = path => createHash('sha256').update(readFileSync(path)).digest('hex');

/** Copy `source` over `target`; a running executable cannot be overwritten but can be renamed, so move it aside first. */
function install(source, target) {
  try {
    copyFileSync(source, target);
  } catch (error) {
    if (error?.code !== 'EBUSY' && error?.code !== 'EPERM') throw error;
    renameSync(target, `${target}.old-${Date.now()}`);
    copyFileSync(source, target);
  }
}
function removeStaleAside(dir) {
  for (const entry of readdirSync(dir)) if (/\.exe\.old-\d+$/.test(entry)) { try { rmSync(join(dir, entry)); } catch { /* still running */ } }
}

/** Promote the executables in `releaseDir` to `stable`; the copy that was there becomes `stable/previous`. */
export function promote(releaseDir, stable, info = {}) {
  const missing = EXECUTABLES.filter(name => !existsSync(join(releaseDir, name)));
  if (missing.length) throw new Error(`The release build is incomplete (missing ${missing.join(', ')}). Run: node tools/build-release.mjs`);
  mkdirSync(stable, { recursive: true });
  // Keep exactly one earlier generation, for rolling back.
  const had = EXECUTABLES.filter(name => existsSync(join(stable, name)));
  if (had.length) {
    const previous = join(stable, 'previous');
    rmSync(previous, { recursive: true, force: true });
    mkdirSync(previous);
    for (const name of had) copyFileSync(join(stable, name), join(previous, name));
    if (existsSync(join(stable, 'VERSION.txt'))) copyFileSync(join(stable, 'VERSION.txt'), join(previous, 'VERSION.txt'));
  }
  for (const name of EXECUTABLES) install(join(releaseDir, name), join(stable, name));
  removeStaleAside(stable);
  const lines = [
    'Agent Workbench, stable copy',
    `promoted: ${new Date().toISOString()}`,
    `commit: ${info.commit ?? 'unknown'}${info.dirty ? ' (working tree had uncommitted changes)' : ''}`,
    ...EXECUTABLES.map(name => `${name}: ${sha(join(stable, name))}  ${statSync(join(stable, name)).size} bytes`),
    '',
    'Run one Agent Workbench at a time (the run history database allows a single app).'
  ];
  writeFileSync(join(stable, 'VERSION.txt'), lines.join('\n') + '\n');
  return lines;
}

/** Put the previous generation back (the current one is dropped). */
export function rollback(stable) {
  const previous = join(stable, 'previous');
  if (!existsSync(previous) || !EXECUTABLES.every(name => existsSync(join(previous, name)))) throw new Error('There is no previous stable copy to go back to.');
  for (const name of EXECUTABLES) install(join(previous, name), join(stable, name));
  if (existsSync(join(previous, 'VERSION.txt'))) copyFileSync(join(previous, 'VERSION.txt'), join(stable, 'VERSION.txt'));
  rmSync(previous, { recursive: true, force: true });
}

function makeShortcut(stable) {
  const link = join(stable, 'Agent Workbench (stable).lnk');
  const exe = join(stable, 'agent-workbench.exe');
  const script = `$s = (New-Object -ComObject WScript.Shell).CreateShortcut('${link}'); $s.TargetPath = '${exe}'; $s.WorkingDirectory = '${stable}'; $s.Description = 'Agent Workbench, stable copy'; $s.Save()`;
  spawnSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', script], { encoding: 'utf8' });
  return existsSync(link) ? link : null;
}

function gitInfo() {
  const run = (...args) => spawnSync('git', args, { cwd: root, encoding: 'utf8' });
  const commit = run('rev-parse', '--short', 'HEAD');
  const status = run('status', '--porcelain');
  return { commit: commit.status === 0 ? commit.stdout.trim() : undefined, dirty: status.status === 0 && status.stdout.trim().length > 0 };
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const stable = stableDir();
    if (process.argv.includes('--rollback')) {
      rollback(stable);
      console.log(`Rolled back to the previous stable copy in ${stable}.`);
    } else {
      const lines = promote(join(root, 'src-tauri', 'target', 'release'), stable, gitInfo());
      const link = makeShortcut(stable);
      console.log(lines.join('\n'));
      console.log(`\nStable copy installed in ${stable}\n${link ? `Shortcut: ${link} (pin it to the taskbar if you like)` : 'Start it with agent-workbench.exe in that folder.'}\nClose a running app before starting the other one.`);
    }
  } catch (e) {
    console.error(`promote-stable: ${e.message}`);
    process.exit(1);
  }
}
