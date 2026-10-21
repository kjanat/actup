// Integration test: verify parseUsesRefs works from Node.js
//
// Run after `wasm-pack build crates/wasm --target nodejs --out-dir ../../target/wasm-pkg`:
//   node crates/wasm/tests/node_parse.mjs

import { parseUsesRefs } from '../../../target/wasm-pkg/actup_wasm.js';

const yaml = `\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: "actions/setup-node@v3"
      - uses: './local/action'
      - uses: org/repo/.github/workflows/ci.yml@main
`;

const results = parseUsesRefs(yaml);

// Validate structure
if (!Array.isArray(results)) {
	throw new Error(`Expected array, got ${typeof results}`);
}

if (results.length !== 4) {
	throw new Error(`Expected 4 results, got ${results.length}: ${JSON.stringify(results)}`);
}

// Check first result: actions/checkout@v4
const first = results[0];
if (first.value !== 'actions/checkout@v4') {
	throw new Error(`Expected value "actions/checkout@v4", got "${first.value}"`);
}
if (first.line !== 7) {
	throw new Error(`Expected line 7, got ${first.line}`);
}
if (typeof first.column !== 'number' || first.column < 1) {
	throw new Error(`Expected positive column, got ${first.column}`);
}
if (typeof first.raw_line !== 'string' || !first.raw_line.includes('uses:')) {
	throw new Error(`Expected raw_line containing "uses:", got "${first.raw_line}"`);
}

// Check second result: actions/setup-node@v3 (quoted)
const second = results[1];
if (second.value !== 'actions/setup-node@v3') {
	throw new Error(`Expected value "actions/setup-node@v3", got "${second.value}"`);
}

// Check third result: local ref
const third = results[2];
if (third.value !== './local/action') {
	throw new Error(`Expected value "./local/action", got "${third.value}"`);
}

// Check fourth result: reusable workflow with subpath
const fourth = results[3];
if (fourth.value !== 'org/repo/.github/workflows/ci.yml@main') {
	throw new Error(`Expected value "org/repo/.github/workflows/ci.yml@main", got "${fourth.value}"`);
}

// Check empty YAML
const empty = parseUsesRefs('');
if (!Array.isArray(empty) || empty.length !== 0) {
	throw new Error(`Expected empty array for empty YAML, got ${JSON.stringify(empty)}`);
}

console.log('PASS: parseUsesRefs returned', results.length, 'results');
console.log('  [0]', JSON.stringify(first));
console.log('  [1]', JSON.stringify(second));
console.log('  [2]', JSON.stringify(third));
console.log('  [3]', JSON.stringify(fourth));
