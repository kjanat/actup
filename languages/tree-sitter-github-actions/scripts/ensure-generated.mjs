#!/usr/bin/env node
// Ensure src/parser.c exists before node-gyp-build runs.
// In published tarballs parser.c is shipped, so we skip.
// In fresh local clones we generate via tree-sitter-cli (a devDep).

import { spawnSync } from 'node:child_process';
import { existsSync } from 'node:fs';

if (existsSync('src/parser.c')) process.exit(0);

const r = spawnSync('tree-sitter', ['generate'], { stdio: 'inherit' });
if (r.status !== 0) {
	console.error('tree-sitter generate failed; install tree-sitter-cli (devDep) to regenerate the parser.');
	process.exit(r.status ?? 1);
}
