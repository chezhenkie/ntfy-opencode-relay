#!/usr/bin/env node
// Every `uses:` in a workflow must name a 40-character commit SHA.
//
// A branch or a tag can move under a release job that holds repository write
// access, so the reference is pinned and the tag stays in a trailing comment.
// Local actions (./path) are allowed. An optional directory argument points the
// scan at a fixture directory.

import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';

const dir = process.argv[2] || '.github/workflows';
const sha = /^[0-9a-f]{40}$/i;
let failures = 0;
let checked = 0;

for (const name of readdirSync(dir).filter((file) => /\.ya?ml$/.test(file)).sort()) {
  const lines = readFileSync(join(dir, name), 'utf8').split('\n');
  lines.forEach((line, index) => {
    const match = line.match(/^\s*(?:-\s+)?uses:\s*(\S+)\s*(?:#.*)?$/);
    if (!match) return;
    const reference = match[1];
    if (reference.startsWith('./')) return;
    const at = reference.lastIndexOf('@');
    const pin = at >= 0 ? reference.slice(at + 1) : '';
    checked += 1;
    if (!sha.test(pin)) {
      failures += 1;
      console.error(`${dir}/${name}:${index + 1}: ${reference} is not pinned to a 40-character SHA`);
    }
  });
}

console.log(`action pins: ${checked} checked, ${failures} unpinned`);
process.exit(failures === 0 ? 0 : 1);
