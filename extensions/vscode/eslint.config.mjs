// @ts-check
/** @see https://eslint.style @see https://typescript-eslint.io */
import js from "@eslint/js";
import stylistic from "@stylistic/eslint-plugin";
import { defineConfig, globalIgnores } from "eslint/config";
import tseslint from "typescript-eslint";

export default defineConfig(
	globalIgnores([
		".vscode-test/",
		"out/",
		"dist/",
	]),
	js.configs.recommended,
	tseslint.configs.recommended,
	tseslint.configs.stylistic,
	{
		plugins: {
			"@stylistic": stylistic,
		},
		rules: {
			"curly": "off",
			"@stylistic/semi": ["warn", "always"],
			"@typescript-eslint/no-empty-function": "off",
			"@typescript-eslint/naming-convention": [
				"warn",
				{
					"selector": "import",
					"format": ["camelCase", "PascalCase"],
				},
			],
			"@typescript-eslint/no-unused-vars": [
				"error",
				{
					"argsIgnorePattern": "^_",
				},
			],
		},
	},
);
