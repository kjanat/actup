import { defineConfig } from "tsdown";

export default defineConfig({
	entry: ["src/web/extension.ts"],
	outDir: "dist/web",
	format: "esm",
	platform: "browser",
	sourcemap: true,
	clean: true,
	external: ["vscode"],
});
