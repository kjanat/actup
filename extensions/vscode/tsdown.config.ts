import { defineConfig } from "tsdown";

export default defineConfig([
	{
		entry: ["src/extension.ts"],
		outDir: "out",
		format: "es",
		platform: "node",
		target: "node22",
		sourcemap: false,
		clean: true,
		minify: "dce-only",
		external: ["vscode"],
	},
	{
		entry: ["src/web/extension.ts"],
		outDir: "dist/web",
		format: "es",
		platform: "browser",
		sourcemap: false,
		clean: true,
		minify: "dce-only",
		external: ["vscode"],
	},
]);
