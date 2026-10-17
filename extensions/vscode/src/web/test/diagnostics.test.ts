import * as wasm from "actup-wasm";
import { describe, expect, mock, test } from "bun:test";
import * as vscode from "vscode";
import { ActupCodeActionProvider } from "../codeActions.ts";
import {
	checkResultsByUri,
	clearDiagnosticData,
	computeDiagnostics,
	createDiagnosticCollection,
	diagnosticDataByUri,
	diagnosticKey,
} from "../diagnostics.ts";
import { codeActionContext, editsOf, FakeDocument, fakeProvider, gate, gatedProvider, rangeOf } from "./fixtures.ts";

const provider = fakeProvider({
	"acme/workflows": [
		{ version: "v1.0.0", sha: "a".repeat(40) },
		{ version: "v1.2.0", sha: "b".repeat(40) },
	],
});

const workflow = [
	"jobs:",
	"  ci:",
	"    uses: github.com/acme/workflows/.github/workflows/ci.yml@v1.0.0",
	"  lint:",
	"    uses: github.com/acme/workflows/.github/workflows/lint.yml@v1.0.0",
	"  gone:",
	"    uses: github.com/acme/missing/.github/workflows/ci.yml@v1.0.0",
	"  escaped:",
	"    uses: \"github.com/acme/missing/.github/workflows/ci.yml@v1.0.\\x30\"",
	"",
].join("\n");

async function diagnose(name: string, text: string): Promise<{ document: FakeDocument; diagnostics: vscode.Diagnostic[] }> {
	const document = new FakeDocument(name, text);
	await computeDiagnostics(document.uri, text, provider, createDiagnosticCollection());
	return { document, diagnostics: vscode.languages.getDiagnostics(document.uri) };
}

describe("computeDiagnostics", () => {
	test("covers the ref of an outdated value and stores its edit data", async () => {
		const { document, diagnostics } = await diagnose("ranges.yml", workflow);
		const [ci, lint] = diagnostics;

		expect(ci?.range).toEqual(rangeOf(document, 2, "v1.0.0"));
		expect(ci?.severity).toBe(vscode.DiagnosticSeverity.Warning);
		expect(ci?.code).toBe("outdated");
		expect(lint?.range).toEqual(rangeOf(document, 4, "v1.0.0"));
		expect([...(diagnosticDataByUri.get(document.uri.toString())?.values() ?? [])]).toEqual([
			{ kind: "outdated", targetRef: "v1.2.0" },
			{ kind: "outdated", targetRef: "v1.2.0" },
		]);
	});

	test("covers the ref of an unresolvable value", async () => {
		const { document, diagnostics } = await diagnose("unresolvable.yml", workflow);
		const gone = diagnostics[2];

		expect(gone?.range).toEqual(rangeOf(document, 6, "v1.0.0"));
		expect(gone?.severity).toBe(vscode.DiagnosticSeverity.Information);
		expect(gone?.code).toBe("unresolvable");
	});

	test("covers the whole line when the line does not hold the value verbatim", async () => {
		const { document, diagnostics } = await diagnose("whole-line.yml", workflow);
		const escaped = diagnostics[3];

		expect(diagnostics).toHaveLength(4);
		expect(escaped?.range).toEqual(document.lineAt(8).range);
		expect(escaped?.code).toBe("unresolvable");
	});
});

describe("ActupCodeActionProvider", () => {
	test("offers edits only for diagnostics with edit data", async () => {
		const { document, diagnostics } = await diagnose("actions.yml", workflow);
		const actions = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(0).range,
			codeActionContext(diagnostics),
		);

		expect(actions.map(action => action.title)).toEqual([
			"Update to v1.2.0",
			"Update to v1.2.0",
			"Fix all outdated actions (2 refs)",
		]);
		const [ciFix, lintFix, fixAll] = actions;
		expect(editsOf(ciFix)).toEqual([{ range: rangeOf(document, 2, "v1.0.0"), newText: "v1.2.0" }]);
		expect(editsOf(lintFix)).toEqual([{ range: rangeOf(document, 4, "v1.0.0"), newText: "v1.2.0" }]);
		expect(editsOf(fixAll)).toEqual([
			{ range: rangeOf(document, 2, "v1.0.0"), newText: "v1.2.0" },
			{ range: rangeOf(document, 4, "v1.0.0"), newText: "v1.2.0" },
		]);
		expect(fixAll?.diagnostics?.map(diagnostic => diagnostic.code)).toEqual(["outdated", "outdated"]);
	});
});

describe("an outdated value the line holds escaped", () => {
	const escapedWorkflow = [
		"jobs:",
		"  ci:",
		"    uses: github.com/acme/workflows/.github/workflows/ci.yml@v1.0.0",
		"  lint:",
		"    uses: github.com/acme/workflows/.github/workflows/lint.yml@v1.0.0",
		"  escaped:",
		"    uses: \"github.com/acme/workflows/.github/workflows/ci.yml@v1.0.\\x30\"",
		"",
	].join("\n");

	test("gets a whole-line diagnostic without edit data", async () => {
		const { document, diagnostics } = await diagnose("escaped-outdated.yml", escapedWorkflow);
		const escaped = diagnostics[2];
		if (escaped === undefined) {
			throw new Error("no diagnostic for the escaped value");
		}
		const dataMap = diagnosticDataByUri.get(document.uri.toString());

		expect(diagnostics).toHaveLength(3);
		expect(escaped.range).toEqual(document.lineAt(6).range);
		expect(escaped.code).toBe("outdated");
		expect(dataMap?.get(diagnosticKey(escaped))).toBeUndefined();
		expect(dataMap?.size).toBe(2);
	});

	test("is skipped by the quick fix and by fix-all", async () => {
		const { document, diagnostics } = await diagnose("escaped-actions.yml", escapedWorkflow);
		const escaped = diagnostics.filter(diagnostic => diagnostic.range.start.line === 6);
		const actions = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(6).range,
			codeActionContext(escaped),
		);

		expect(escaped).toHaveLength(1);
		expect(actions.map(action => action.title)).toEqual(["Fix all outdated actions (2 refs)"]);
		expect(editsOf(actions[0])).toEqual([
			{ range: rangeOf(document, 2, "v1.0.0"), newText: "v1.2.0" },
			{ range: rangeOf(document, 4, "v1.0.0"), newText: "v1.2.0" },
		]);
	});
});

describe("a check that finishes after a newer computation or a close", () => {
	function reusable(ref: string): string {
		return `jobs:\n  ci:\n    uses: acme/workflows/.github/workflows/ci.yml@${ref}\n`;
	}

	function storedValues(uri: vscode.Uri): string[] | undefined {
		return checkResultsByUri.get(uri.toString())?.map(result => result.location.value);
	}

	test("leaves the newer results in place", async () => {
		const uri = vscode.Uri.parse("file:///stale-success.yml");
		const collection = createDiagnosticCollection();
		const held = gate();
		const older = computeDiagnostics(uri, reusable("v1.0.0"), gatedProvider(provider, held), collection);
		await computeDiagnostics(uri, reusable("v1.2.0"), provider, collection);
		held.open();
		await older;

		expect(collection.get(uri)).toEqual([]);
		expect(storedValues(uri)).toEqual(["acme/workflows/.github/workflows/ci.yml@v1.2.0"]);
		expect(diagnosticDataByUri.get(uri.toString())?.size).toBe(0);
	});

	test("leaves the newer results in place when the older check fails", async () => {
		const uri = vscode.Uri.parse("file:///stale-failure.yml");
		const collection = createDiagnosticCollection();
		const held = gate();
		const real = { ...wasm };
		let calls = 0;
		await mock.module("actup-wasm", () => ({
			...real,
			checkRefs: (text: string, p: wasm.ActupProvider) =>
				calls++ === 0
					? held.opened.then(() => Promise.reject(new Error("offline")))
					: real.checkRefs(text, p),
		}));
		try {
			const older = computeDiagnostics(uri, reusable("v1.0.0"), provider, collection);
			await computeDiagnostics(uri, reusable("v1.0.0"), provider, collection);
			held.open();
			await older;
		} finally {
			await mock.module("actup-wasm", () => real);
		}

		expect(collection.get(uri)?.map(diagnostic => diagnostic.code)).toEqual(["outdated"]);
		expect(storedValues(uri)).toEqual(["acme/workflows/.github/workflows/ci.yml@v1.0.0"]);
		expect(diagnosticDataByUri.get(uri.toString())?.size).toBe(1);
	});

	test("writes nothing once the document has closed", async () => {
		const uri = vscode.Uri.parse("file:///stale-closed.yml");
		const collection = createDiagnosticCollection();
		const held = gate();
		const older = computeDiagnostics(uri, reusable("v1.0.0"), gatedProvider(provider, held), collection);
		clearDiagnosticData(uri);
		held.open();
		await older;

		expect(collection.get(uri)).toBeUndefined();
		expect(checkResultsByUri.has(uri.toString())).toBe(false);
		expect(diagnosticDataByUri.has(uri.toString())).toBe(false);
	});
});
