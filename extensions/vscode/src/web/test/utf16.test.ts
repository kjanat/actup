import { describe, expect, test } from "bun:test";
import * as vscode from "vscode";
import { ActupCodeActionProvider } from "../codeActions.ts";
import { computeDiagnostics, createDiagnosticCollection } from "../diagnostics.ts";
import { fixAllInWorkspace } from "../extension.ts";
import { ActupHoverProvider } from "../hover.ts";
import { codeActionContext, editsOf, FakeDocument, FakeLogOutputChannel, fakeProvider, rangeOf } from "./fixtures.ts";
import { window as mockWindow, workspace as mockWorkspace } from "./vscode.ts";

const tags = [
	{ version: "v1.0.0", sha: "a".repeat(40) },
	{ version: "v1.2.0", sha: "b".repeat(40) },
];
const provider = fakeProvider({ "actions/checkout": tags, "acme/workflows": tags });

const text = [
	"jobs:",
	"  build:",
	"    steps:",
	"      - { name: \"✓ 🚀 build\", uses: actions/checkout@v1.0.0 }",
	"  ci: { name: \"✓ 🚀 ci\", uses: github.com/acme/workflows/.github/workflows/ci.yml@v1.0.0 }",
	"",
].join("\n");

describe("multi-byte text before `uses:`", () => {
	test("the diagnostic and its quick fix cover exactly the ref", async () => {
		const document = new FakeDocument("utf16-diagnostics.yml", text);
		await computeDiagnostics(document.uri, text, provider, createDiagnosticCollection());
		const diagnostics = vscode.languages.getDiagnostics(document.uri).filter(diagnostic => diagnostic.range.start.line === 4);
		const [quickFix] = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(4).range,
			codeActionContext(diagnostics),
		);

		expect(diagnostics.map(diagnostic => diagnostic.range)).toEqual([rangeOf(document, 4, "v1.0.0")]);
		expect(editsOf(quickFix)).toEqual([{ range: rangeOf(document, 4, "v1.0.0"), newText: "v1.2.0" }]);
	});

	test("the hover covers exactly the value", async () => {
		const document = new FakeDocument("utf16-hover.yml", text);
		const hover = await new ActupHoverProvider(provider, new Map()).provideHover(
			document,
			new vscode.Position(3, 40),
		);

		expect(hover?.range).toEqual(rangeOf(document, 3, "actions/checkout@v1.0.0"));
	});

	test("fix all in workspace replaces exactly the refs", async () => {
		mockWorkspace.reset();
		mockWindow.reset();
		const document = new FakeDocument("repo/.github/workflows/utf16.yml", text);
		mockWorkspace.documents.push(document);
		await fixAllInWorkspace(provider, new FakeLogOutputChannel());
		const [applied] = mockWorkspace.appliedEdits;

		expect(mockWorkspace.appliedEdits).toHaveLength(1);
		expect(applied?.entries().flatMap(([, edits]) => edits)).toEqual([
			new vscode.TextEdit(rangeOf(document, 3, "v1.0.0"), "v1.2.0"),
			new vscode.TextEdit(rangeOf(document, 4, "v1.0.0"), "v1.2.0"),
		]);
		expect(mockWindow.messages).toEqual(["actup: Updated 2 action ref(s) across 1 file(s)."]);
	});
});
