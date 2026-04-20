#!/usr/bin/env bun
import { $ } from "bun";
import { join, resolve } from "node:path";

const pkgDir = resolve(import.meta.dirname, "..");
const repoRoot = resolve(pkgDir, "../..");
const outDir = join(pkgDir, "wasm");

// Copy license files from repo root to cwd
console.log("Copying license files...");
await $`cp LICENSE* ${pkgDir}/`.cwd(repoRoot);

console.log("Building actup-wasm (--target web)...");
await $`wasm-pack build ${join(repoRoot, "crates/wasm")} --target web --out-dir ${outDir} --out-name actup_wasm`;

await $`rm -f package.json .gitignore`.cwd(outDir);

const wasm = Bun.file(join(outDir, "actup_wasm_bg.wasm"));
if (await wasm.exists()) {
	const raw = wasm.size;
	const gz = Bun.gzipSync(await wasm.bytes()).length;
	console.log(`WASM size: ${raw} bytes raw, ${gz} bytes gzipped`);
	const MAX_GZ = 512_000;
	if (gz > MAX_GZ) {
		console.error(`ERROR: gzipped WASM exceeds ${MAX_GZ} bytes (${gz})`);
		process.exit(1);
	}
}

console.log(`Done. Output: ${outDir}`);
