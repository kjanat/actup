import * as wasm from "actup-wasm";
import { describe, expect, mock, test } from "bun:test";
import * as vscode from "vscode";
import { checkResultsByUri, clearDiagnosticData, computeDiagnostics, createDiagnosticCollection } from "../diagnostics.ts";
import { ActupHoverProvider } from "../hover.ts";
import { FakeDocument, fakeProvider } from "./fixtures.ts";

const provider = fakeProvider({
	"actions/checkout": [
		{ version: "v1.0.0", sha: "a".repeat(40) },
		{ version: "v1.2.0", sha: "b".repeat(40) },
	],
});

function workflow(ref: string): string {
	return `jobs:\n  build:\n    steps:\n      - uses: actions/checkout@${ref}\n`;
}

async function statusRows(document: FakeDocument): Promise<string[]> {
	const hover = await new ActupHoverProvider(provider, checkResultsByUri).provideHover(
		document,
		new vscode.Position(3, 20),
	);
	return (hover?.contents ?? [])
		.flatMap(content => (typeof content === "string" ? content : content.value).split("\n"))
		.filter(row => row.startsWith("$("));
}

describe("hover status from computed diagnostics", () => {
	test("shows the status that computeDiagnostics stored", async () => {
		const document = new FakeDocument("hover-status.yml", workflow("v1.0.0"));
		await computeDiagnostics(document.uri, document.getText(), provider, createDiagnosticCollection());

		expect(await statusRows(document)).toEqual(["$(warning) Outdated: `v1.0.0` → `v1.2.0`"]);
	});

	test("shows the status of the latest computation", async () => {
		const collection = createDiagnosticCollection();
		const stale = new FakeDocument("hover-recompute.yml", workflow("v1.0.0"));
		await computeDiagnostics(stale.uri, stale.getText(), provider, collection);
		const current = new FakeDocument("hover-recompute.yml", workflow("v1.2.0"));
		await computeDiagnostics(current.uri, current.getText(), provider, collection);

		expect(await statusRows(current)).toEqual(["$(check) Up to date at `v1.2.0`"]);
	});

	test("drops the stored results when the document closes", async () => {
		const document = new FakeDocument("hover-closed.yml", workflow("v1.0.0"));
		await computeDiagnostics(document.uri, document.getText(), provider, createDiagnosticCollection());
		clearDiagnosticData(document.uri);

		expect(checkResultsByUri.has(document.uri.toString())).toBe(false);
		expect(await statusRows(document)).toEqual([]);
	});

	test("drops the stored results when the text loses its `uses:` keys", async () => {
		const collection = createDiagnosticCollection();
		const document = new FakeDocument("hover-no-uses.yml", workflow("v1.0.0"));
		await computeDiagnostics(document.uri, document.getText(), provider, collection);
		await computeDiagnostics(document.uri, "jobs: {}\n", provider, collection);

		expect(checkResultsByUri.has(document.uri.toString())).toBe(false);
	});

	test("drops the stored results when the check fails", async () => {
		const collection = createDiagnosticCollection();
		const document = new FakeDocument("hover-check-failed.yml", workflow("v1.0.0"));
		await computeDiagnostics(document.uri, document.getText(), provider, collection);
		const real = { ...wasm };
		await mock.module("actup-wasm", () => ({ ...real, checkRefs: () => Promise.reject(new Error("offline")) }));
		try {
			await computeDiagnostics(document.uri, document.getText(), provider, collection);
		} finally {
			await mock.module("actup-wasm", () => real);
		}

		expect(checkResultsByUri.has(document.uri.toString())).toBe(false);
		expect(collection.get(document.uri)).toEqual([]);
	});
});
