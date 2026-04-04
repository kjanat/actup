import { defineConfig } from "tsdown";

export default defineConfig({
	entry: ["src/web/extension.ts"],
	outDir: "dist/web",
	format: "esm",
	platform: "browser",
	sourcemap: true,
	clean: true,
	external: ["vscode"],
	// node:fs and node:url are imported by actup-wasm's Node.js code path
	// but never reached in browser context (we pass wasmSource explicitly)
	resolve: {
		alias: {
			"node:fs": "./src/web/shims/empty.ts",
			"node:url": "./src/web/shims/empty.ts",
		},
	},
});
