import { describe, expect, test } from "bun:test";
import * as vscode from "vscode";
import { ActupCodeActionProvider } from "../codeActions.ts";
import { computeDiagnostics, createDiagnosticCollection, diagnosticDataByUri } from "../diagnostics.ts";
import { codeActionContext, editsOf, FakeDocument, fakeProvider, rangeOf } from "./fixtures.ts";

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
