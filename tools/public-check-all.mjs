// Public source checks. No private backup/agent configuration is required.
// Native e2e is separate: it must run from a normal console that permits hosts.
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';
const root = fileURLToPath(new URL('../', import.meta.url));
const rows = [];
function run(name, command, args, cwd = root) {
  const quoted = process.platform === 'win32' && /\s/.test(command) ? `"${command}"` : command;
  const r = spawnSync(quoted, args, {cwd, shell:process.platform === 'win32', stdio:'inherit'});
  rows.push({name,status:r.status === 0 ? 'PASS' : 'FAIL'});
}
run('privacy tooling (offline)',process.env.PYTHON ?? 'python',['-m','unittest','tools.public_release.test_public_release','-v']);
run('frontend units',process.execPath,['tools/frontend-tests/run.mjs']);
run('frontend build/type check','npm',['--prefix','frontend','run','build']);
if (process.platform === 'win32') {
  const target=['--locked','--target','x86_64-pc-windows-msvc'];
  run('observer',join(root,'crates/wb-observe/cargo-vs.cmd'),['test',...target],join(root,'crates/wb-observe'));
  run('native app',join(root,'src-tauri/check-vs2022.cmd'),['test',...target]);
  run('service units',join(root,'crates/wb-session/cargo-vs.cmd'),['test','--lib',...target],join(root,'crates/wb-session'));
} else rows.push({name:'native Windows tests',status:'SKIP'});
if (!process.argv.includes('--quick')) run('headless UI','npm',['--prefix','frontend','run','test:headless']);
else rows.push({name:'headless UI',status:'SKIP'});
console.table(rows);
console.log('Service end-to-end and real desktop acceptance are separate; do not count skips as acceptance.');
process.exit(rows.some(r=>r.status === 'FAIL')?1:0);
