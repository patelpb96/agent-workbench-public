// Tests of the stable-copy tool: promote, keep one previous generation, roll back, refuse an incomplete build.
import assert from 'node:assert/strict';
import { existsSync, mkdirSync, mkdtempSync, readFileSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';
import { EXECUTABLES, promote, rollback } from './promote-stable.mjs';

const scratch = () => mkdtempSync(join(tmpdir(), 'awstable-'));
function release(tag) {
  const dir = scratch();
  for (const name of EXECUTABLES) writeFileSync(join(dir, name), `${name} ${tag}`);
  return dir;
}

test('promoting copies every executable, records what it is, and keeps one earlier generation', () => {
  const stable = join(scratch(), 'stable');
  const lines = promote(release('v1'), stable, { commit: 'abc1234', dirty: false });
  for (const name of EXECUTABLES) assert.equal(readFileSync(join(stable, name), 'utf8'), `${name} v1`);
  const version = readFileSync(join(stable, 'VERSION.txt'), 'utf8');
  assert.match(version, /commit: abc1234\n/);
  assert.match(version, /agent-workbench\.exe: [0-9a-f]{64}/);
  assert.ok(lines.length > 5);
  assert.ok(!existsSync(join(stable, 'previous')), 'nothing to keep the first time');

  promote(release('v2'), stable, { commit: 'def5678', dirty: true });
  assert.equal(readFileSync(join(stable, 'agent-workbench.exe'), 'utf8'), 'agent-workbench.exe v2');
  assert.equal(readFileSync(join(stable, 'previous', 'agent-workbench.exe'), 'utf8'), 'agent-workbench.exe v1');
  assert.match(readFileSync(join(stable, 'VERSION.txt'), 'utf8'), /def5678 \(working tree had uncommitted changes\)/);

  promote(release('v3'), stable, {});
  assert.equal(readFileSync(join(stable, 'previous', 'wb.exe'), 'utf8'), 'wb.exe v2', 'only the last generation is kept');
});

test('rolling back restores the previous copy, and fails cleanly when there is none', () => {
  const stable = join(scratch(), 'stable');
  assert.throws(() => rollback(stable), /no previous/);
  promote(release('v1'), stable, { commit: 'aaa' });
  assert.throws(() => rollback(stable), /no previous/);
  promote(release('v2'), stable, { commit: 'bbb' });
  rollback(stable);
  for (const name of EXECUTABLES) assert.equal(readFileSync(join(stable, name), 'utf8'), `${name} v1`);
  assert.match(readFileSync(join(stable, 'VERSION.txt'), 'utf8'), /commit: aaa/);
  assert.ok(!existsSync(join(stable, 'previous')));
});

test('an incomplete release build is refused and changes nothing', () => {
  const stable = join(scratch(), 'stable');
  const partial = scratch();
  writeFileSync(join(partial, 'agent-workbench.exe'), 'x');
  assert.throws(() => promote(partial, stable, {}), /incomplete.*wb-daemon\.exe/);
  assert.ok(!existsSync(join(stable, 'agent-workbench.exe')));
  mkdirSync(stable, { recursive: true });
});
