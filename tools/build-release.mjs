// One-step local release build: production frontend bundle FIRST, then the
// locked native release that embeds it. Building only the native side can ship
// a stale UI with an unchanged executable hash (observed 2026-10-21).
// Usage (repo root): node tools/build-release.mjs
import { spawnSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import { copyFileSync, readFileSync, readdirSync, renameSync, statSync, unlinkSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = fileURLToPath(new URL('../', import.meta.url));
const run = (command, args, options = {}) => {
  // With shell:true cmd.exe splits an unquoted path at its first space.
  const quoted = process.platform === 'win32' && /\s/.test(command) ? `"${command}"` : command;
  const result = spawnSync(quoted, args, { cwd: root, stdio: 'inherit', shell: process.platform === 'win32', ...options });
  if (result.error) {
    console.error(`Could not run ${command}: ${result.error.message}. Is the prerequisite installed and on PATH?`);
    process.exit(1);
  }
  if (result.status !== 0) process.exit(result.status ?? 1);
};
run('npm', ['--prefix', 'frontend', 'run', 'build']);
const assets = join(root, 'frontend', 'dist', 'assets');
for (const name of readdirSync(assets).filter(name => name.endsWith('.js'))) {
  // Production bundles must never contain the headless test transport.
  if (readFileSync(join(assets, name), 'utf8').includes('__headlessInvoke')) {
    console.error(`Refusing release: headless test hook found in ${name}`);
    process.exit(1);
  }
}
run(join(root, 'src-tauri', 'check-vs2022.cmd'), ['build', '--release', '--locked']);
// The persistent-session service ships next to the app (started on demand, outlives it).
run(join(root, 'crates', 'wb-session', 'cargo-vs.cmd'), ['build', '--release', '--locked', '--target', 'x86_64-pc-windows-msvc'], { cwd: join(root, 'crates', 'wb-session') });
const releaseDir = join(root, 'src-tauri', 'target', 'release');
// Leftovers from earlier updates of a running service: delete what is no longer in use, skip what still is.
for (const entry of readdirSync(releaseDir)) {
  if (/^wb-(daemon|host)\.exe\.old-\d+$/.test(entry)) { try { unlinkSync(join(releaseDir, entry)); } catch { /* still running */ } }
}
for (const name of ['wb-daemon.exe', 'wb-host.exe', 'wb.exe']) {
  const source = join(root, 'crates', 'wb-session', 'target', 'x86_64-pc-windows-msvc', 'release', name);
  const target = join(releaseDir, name);
  try {
    copyFileSync(source, target);
  } catch (error) {
    if (error?.code !== 'EBUSY' && error?.code !== 'EPERM') throw error;
    // A running service locks its executable, but Windows lets a running executable be RENAMED. Move it aside
    // and put the new one in its place: terminals started from now on use the new host, existing terminals and
    // the running daemon are not touched (stopping the daemon never ends terminals, but this script will not do it).
    renameSync(target, `${target}.old-${Date.now()}`);
    copyFileSync(source, target);
    console.warn(`${name} is running: the new build was installed beside it. New terminals use it now${name === 'wb-daemon.exe' ? '; restart the service (not the terminals) to run the new daemon' : ''}.`);
  }
}
const exe = join(root, 'src-tauri', 'target', 'release', 'agent-workbench.exe');
const bytes = readFileSync(exe);
console.log(JSON.stringify({
  executable: exe,
  bytes: statSync(exe).size,
  sha256: createHash('sha256').update(bytes).digest('hex'),
}));
