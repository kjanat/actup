import { checkRefs } from "actup-wasm";
import { describe, expect, test } from "bun:test";
import * as vscode from "vscode";
import { computeDiagnostics, createDiagnosticCollection } from "../diagnostics.ts";
import { ActupHoverProvider } from "../hover.ts";
import { FakeDocument, fakeProvider, rangeOf } from "./fixtures.ts";

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
	"      - uses: actions/checkout/sub/dir@v1.0.0",
	"      - uses: GitHub.COM/actions/checkout@v1.0.0",
	"      - uses: actions/checkout@v1.0.0",
	"",
].join("\n");

function hoverText(hover: vscode.Hover | undefined): string {
	return (hover?.contents ?? []).map(content => typeof content === "string" ? content : content.value).join("");
}

describe("results matched to located actions", () => {
	test("give every remote ref its own diagnostic", async () => {
		const document = new FakeDocument("matching.yml", text);
		await computeDiagnostics(document.uri, text, provider, createDiagnosticCollection());

		expect(vscode.languages.getDiagnostics(document.uri).map(diagnostic => diagnostic.range)).toEqual([
			rangeOf(document, 4, "v1.0.0"),
			rangeOf(document, 5, "v1.0.0"),
			rangeOf(document, 6, "v1.0.0"),
			rangeOf(document, 7, "v1.0.0"),
			rangeOf(document, 8, "v1.0.0"),
		]);
	});

	test("give the hover the status of the ref under the cursor", async () => {
		const document = new FakeDocument("matching-hover.yml", text);
		const results = await checkRefs(text, provider);
		const hover = new ActupHoverProvider(provider, new Map([[document.uri.toString(), results]]));

		for (const line of [4, 5, 6, 7, 8]) {
			expect(hoverText(await hover.provideHover(document, new vscode.Position(line, 20)))).toContain(
				"Outdated: `v1.0.0` → `v1.2.0`",
			);
		}
	});
});
