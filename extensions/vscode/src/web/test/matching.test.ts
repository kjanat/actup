import { describe, expect, test } from "bun:test";
import * as vscode from "vscode";
import { noConfig } from "../config.ts";
import { checkResultsByUri, computeDiagnostics, createDiagnosticCollection } from "../diagnostics.ts";
import { fixAllInWorkspace } from "../extension.ts";
import { ActupHoverProvider } from "../hover.ts";
import { FakeDocument, FakeLogOutputChannel, fakeProvider, hoverStatusRows, rangeOf } from "./fixtures.ts";
import { window as mockWindow, workspace as mockWorkspace } from "./vscode.ts";

const provider = fakeProvider({
	"actions/checkout": [
		{ version: "v1.0.0", sha: "a".repeat(40) },
		{ version: "v1.2.0", sha: "b".repeat(40) },
	],
});

const text = [
	"jobs:",
	"  build:",
	"    steps:",
	"      - uses: ./local/action",
	"      - uses: actions/checkout@v1.0.0",
	"      - uses: github.com/actions/checkout@v1.0.0",
	"      - uses: actions/checkout/sub/dir@v1.2.0",
	"      - uses: GitHub.COM/actions/checkout@v1.0.0",
	"      - uses: actions/checkout@v1.0.0",
	"",
].join("\n");

describe("results matched to located actions", () => {
	test("give every outdated remote ref its own diagnostic", async () => {
		const document = new FakeDocument("matching.yml", text);
		await computeDiagnostics(document.uri, text, provider, createDiagnosticCollection());

		expect(vscode.languages.getDiagnostics(document.uri).map(diagnostic => diagnostic.range)).toEqual([
			rangeOf(document, 4, "v1.0.0"),
			rangeOf(document, 5, "v1.0.0"),
			rangeOf(document, 7, "v1.0.0"),
			rangeOf(document, 8, "v1.0.0"),
		]);
	});

	test("give the hover the status of the ref under the cursor", async () => {
		const document = new FakeDocument("matching-hover.yml", text);
		await computeDiagnostics(document.uri, text, provider, createDiagnosticCollection());
		const hover = new ActupHoverProvider(provider, checkResultsByUri);
		const statusByLine = new Map([
			[4, "**Outdated**: v1.0.0 -> v1.2.0 (`bbbbbbb`)"],
			[5, "**Outdated**: v1.0.0 -> v1.2.0 (`bbbbbbb`)"],
			[6, "Up to date"],
			[7, "**Outdated**: v1.0.0 -> v1.2.0 (`bbbbbbb`)"],
			[8, "**Outdated**: v1.0.0 -> v1.2.0 (`bbbbbbb`)"],
		]);

		for (const [line, status] of statusByLine) {
			expect(hoverStatusRows(await hover.provideHover(document, new vscode.Position(line, 20)))).toEqual([status]);
		}
	});

	test("give fix all in workspace one edit per repeated ref", async () => {
		mockWorkspace.reset();
		mockWindow.reset();
		const repeated = [
			"jobs:",
			"  build:",
			"    steps:",
			"      - uses: actions/checkout@v1.0.0",
			"      - uses: actions/checkout@v1.0.0",
			"",
		].join("\n");
		const document = new FakeDocument("repo/.github/workflows/repeated.yml", repeated);
		mockWorkspace.documents.push(document);
		await fixAllInWorkspace(provider, new FakeLogOutputChannel(), noConfig);

		expect(mockWorkspace.appliedEdits.flatMap(edit => edit.entries().flatMap(([, edits]) => edits))).toEqual([
			new vscode.TextEdit(rangeOf(document, 3, "v1.0.0"), "v1.2.0"),
			new vscode.TextEdit(rangeOf(document, 4, "v1.0.0"), "v1.2.0"),
		]);
		expect(mockWindow.messages).toEqual(["actup: Updated 2 action ref(s) across 1 file(s)."]);
	});

	test("give fix all in workspace a re-pin of a SHA and its version comment", async () => {
		mockWorkspace.reset();
		mockWindow.reset();
		const pinned = [
			"jobs:",
			"  build:",
			"    steps:",
			`      - uses: actions/checkout@${"a".repeat(40)} # v1.0.0`,
			"",
		].join("\n");
		const document = new FakeDocument("repo/.github/workflows/pinned.yml", pinned);
		mockWorkspace.documents.push(document);
		await fixAllInWorkspace(provider, new FakeLogOutputChannel(), noConfig);
		const sha = rangeOf(document, 3, "a".repeat(40));

		expect(mockWorkspace.appliedEdits.flatMap(edit => edit.entries().flatMap(([, edits]) => edits))).toEqual([
			new vscode.TextEdit(
				new vscode.Range(3, sha.start.character, 3, document.lineAt(3).text.length),
				`${"b".repeat(40)} # v1.2.0`,
			),
		]);
		expect(mockWindow.messages).toEqual(["actup: Updated 1 action ref(s) across 1 file(s)."]);
	});
});
