import * as esbuild from "esbuild";
import { cpSync, mkdirSync } from "node:fs";

const watch = process.argv.includes("--watch");

// Copy WASM binary into the web extension output
mkdirSync("dist/web/wasm", { recursive: true });
cpSync(
	"../../packages/actup-wasm/wasm/actup_wasm_bg.wasm",
	"dist/web/wasm/actup_wasm_bg.wasm",
);

const ctx = await esbuild.context({
	entryPoints: ["src/web/extension.ts"],
	bundle: true,
	outfile: "dist/web/extension.js",
	external: ["vscode"],
	format: "cjs",
	platform: "browser",
	sourcemap: true,
	// node:fs and node:url are imported by actup-wasm's Node.js code path
	// but never reached in browser context (we pass wasmSource explicitly)
	alias: {
		"node:fs": "./src/web/shims/empty.ts",
		"node:url": "./src/web/shims/empty.ts",
	},
});

if (watch) {
	await ctx.watch();
	console.log("Watching for changes...");
} else {
	await ctx.rebuild();
	await ctx.dispose();
}
