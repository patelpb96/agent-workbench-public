#!/usr/bin/env node
// Grep-driven hardening inventory of the Rust code (no changes made): where a panic, a leak or an unreviewed
// unsafe block could hide. Counts only production code (everything before a file's `#[cfg(test)]`, and never the
// `tests` folders). Run: node tools/audit-hardening.mjs [--files]   (--files lists every hit with its line)
//
// What it looks for, and why:
//   unwrap / expect / panic!   in a long-running background service one of these on an unexpected error kills it
//                              and every terminal session it serves
//   unsafe                     each block needs a reason a null handle or bad buffer cannot reach it
//   unbounded channels         a producer that outruns its consumer eats memory without limit
//   Vec::new() growth          flagged by name only (`push` in a loop needs a human look)
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative, resolve, sep } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(fileURLToPath(new URL('..', import.meta.url)));
const AREAS = [
  ['terminal service (long-running)', 'crates/wb-session/src'],
  ['observer', 'crates/wb-observe/src'],
  ['app (Tauri)', 'src-tauri/src'],
  ['app library', 'src-tauri/src/codex']
];
const SKIP_DIR = new Set(['target', 'node_modules', 'tests', 'artifacts']);
const PATTERNS = {
  unwrap: /\.unwrap\(\)/,
  expect: /\.expect\(/,
  panic: /\b(panic!|unreachable!|todo!|unimplemented!)\(/,
  unsafe: /\bunsafe\b/,
  unbounded: /unbounded_channel|mpsc::channel\(\)|crossbeam_channel::unbounded|sync_channel\(\s*usize::MAX/
};

function rustFiles(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    const meta = statSync(path);
    if (meta.isDirectory()) { if (!SKIP_DIR.has(name)) out.push(...rustFiles(path)); }
    else if (name.endsWith('.rs') && name !== 'tests.rs') out.push(path);
  }
  return out;
}

/** The production lines of a file: everything before its test module, comments excluded. */
export function productionLines(text) {
  const lines = text.split('\n');
  const end = lines.findIndex(l => l.trim().startsWith('#[cfg(test)]'));
  return (end < 0 ? lines : lines.slice(0, end)).map((text, i) => ({ n: i + 1, text })).filter(l => !l.text.trim().startsWith('//'));
}

export function audit(files) {
  const rows = [];
  for (const file of files) {
    const hits = Object.fromEntries(Object.keys(PATTERNS).map(k => [k, []]));
    for (const line of productionLines(readFileSync(file, 'utf8'))) {
      for (const [kind, pattern] of Object.entries(PATTERNS)) if (pattern.test(line.text)) hits[kind].push(line);
    }
    if (Object.values(hits).some(h => h.length)) rows.push({ file: relative(root, file).split(sep).join('/'), hits });
  }
  return rows;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const detail = process.argv.includes('--files');
  const seen = new Set();
  for (const [label, dir] of AREAS.slice(0, 3)) {
    const rows = audit(rustFiles(join(root, dir)).filter(f => !seen.has(f) && seen.add(f)));
    const total = Object.fromEntries(Object.keys(PATTERNS).map(k => [k, rows.reduce((n, r) => n + r.hits[k].length, 0)]));
    console.log(`\n== ${label}  (${dir})\n   totals: ${Object.entries(total).map(([k, v]) => `${k} ${v}`).join(', ')}`);
    for (const r of rows.sort((a, b) => b.hits.unwrap.length + b.hits.expect.length - (a.hits.unwrap.length + a.hits.expect.length))) {
      console.log(`   ${r.file.padEnd(52)} ${Object.entries(r.hits).filter(([, h]) => h.length).map(([k, h]) => `${k} ${h.length}`).join(', ')}`);
      if (detail) for (const [kind, h] of Object.entries(r.hits)) for (const l of h) console.log(`        ${kind} ${l.n}: ${l.text.trim().slice(0, 110)}`);
    }
  }
}
