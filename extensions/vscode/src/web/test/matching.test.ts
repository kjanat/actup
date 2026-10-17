import { describe, expect, test } from "bun:test";
import * as vscode from "vscode";
import { checkResultsByUri, computeDiagnostics, createDiagnosticCollection } from "../diagnostics.ts";
import { fixAllInWorkspace } from "../extension.ts";
import { ActupHoverProvider } from "../hover.ts";
import { FakeDocument, FakeLogOutputChannel, fakeProvider, rangeOf } from "./fixtures.ts";
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

function hoverText(hover: vscode.Hover | undefined): string {
	return (hover?.contents ?? []).map(content => typeof content === "string" ? content : content.value).join("");
}

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
			[4, "$(warning) Outdated: `v1.0.0` → `v1.2.0`"],
			[5, "$(warning) Outdated: `v1.0.0` → `v1.2.0`"],
			[6, "$(check) Up to date at `v1.2.0`"],
			[7, "$(warning) Outdated: `v1.0.0` → `v1.2.0`"],
			[8, "$(warning) Outdated: `v1.0.0` → `v1.2.0`"],
		]);

		for (const [line, status] of statusByLine) {
			const shown = hoverText(await hover.provideHover(document, new vscode.Position(line, 20)));
			expect(shown.split("\n").filter(row => row.startsWith("$("))).toEqual([status]);
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
		await fixAllInWorkspace(provider, new FakeLogOutputChannel());

		expect(mockWorkspace.appliedEdits.flatMap(edit => edit.entries().flatMap(([, edits]) => edits))).toEqual([
			new vscode.TextEdit(rangeOf(document, 3, "v1.0.0"), "v1.2.0"),
			new vscode.TextEdit(rangeOf(document, 4, "v1.0.0"), "v1.2.0"),
		]);
		expect(mockWindow.messages).toEqual(["actup: Updated 2 action ref(s) across 1 file(s)."]);
	});
});
