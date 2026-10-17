import assert from "node:assert/strict";
import file from "node:fs/promises";
import { resolve } from "node:path";
import { test } from "node:test";
import ts from "typescript";

const declarationsPath = resolve(import.meta.dirname, "../dist/index.d.ts");
try {
	await file.access(declarationsPath);
} catch (err) {
	console.error(err.message, `\nPlease run \`bun run build\` first.`);
	process.exit(1);
}

const consumer = resolve(import.meta.dirname, "consumer.ts");
const options = {
	module: ts.ModuleKind.ES2022,
	moduleResolution: ts.ModuleResolutionKind.Bundler,
	noEmit: true,
	skipLibCheck: true,
	strict: true,
	target: ts.ScriptTarget.ES2022,
	types: [],
};

function diagnose(source) {
	const host = ts.createCompilerHost(options);
	const { fileExists, getSourceFile, readFile } = host;
	host.fileExists = (name) => name === consumer || fileExists(name);
	host.readFile = (name) => (name === consumer ? source : readFile(name));
	host.getSourceFile = (name, version, ...rest) =>
		name === consumer
			? ts.createSourceFile(name, source, version, true)
			: getSourceFile(name, version, ...rest);
	const program = ts.createProgram([consumer], options, host);
	return ts.getPreEmitDiagnostics(program, program.getSourceFile(consumer)).map((d) => ({
		code: d.code,
		message: ts.flattenDiagnosticMessageText(d.messageText, "\n"),
	}));
}

test("ArchivedPolicy accepts every archived policy value", () => {
	const diagnostics = diagnose(`
import type { ArchivedPolicy } from "../dist/index.js";
export const hint: ArchivedPolicy = "hint";
export const ignore: ArchivedPolicy = "ignore";
`);
	assert.deepEqual(diagnostics, []);
});

test("ArchivedPolicy rejects other strings", () => {
	const diagnostics = diagnose(`
import type { ArchivedPolicy } from "../dist/index.js";
export const warn: ArchivedPolicy = "warn";
`);
	assert.deepEqual(
		diagnostics.map((d) => d.code),
		[2322],
		JSON.stringify(diagnostics),
	);
});

test("RefKind and RefRange annotate check results and uses locations", () => {
	const diagnostics = diagnose(`
import type { LocatedCheckResult, RefKind, RefRange, UsesLocation } from "../dist/index.js";
export const kinds: RefKind[] = ["tag", "branch", "sha"];
export const kindOf = (result: LocatedCheckResult): RefKind | null => result.ref_kind;
export const range: RefRange = { start: 0, end: 4 };
export const refRangeOf = (location: UsesLocation): RefRange | null => location.ref_range;
`);
	assert.deepEqual(diagnostics, []);
});

test("RefKind and RefRange reject other shapes", () => {
	const diagnostics = diagnose(`
import type { RefKind, RefRange } from "../dist/index.js";
export const kind: RefKind = "commit";
export const range: RefRange = { start: 0 };
`);
	assert.deepEqual(
		diagnostics.map((d) => d.code),
		[2322, 2741],
		JSON.stringify(diagnostics),
	);
});
