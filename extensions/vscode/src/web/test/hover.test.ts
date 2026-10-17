import * as wasm from "actup-wasm";
import { describe, expect, mock, test } from "bun:test";
import * as vscode from "vscode";
import { checkResultsByUri, clearDiagnosticData, computeDiagnostics, createDiagnosticCollection } from "../diagnostics.ts";
import { ActupHoverProvider } from "../hover.ts";
import { FakeDocument, fakeProvider, rangeOf } from "./fixtures.ts";

const tags = [
	{ version: "v1.0.0", sha: "a".repeat(40) },
	{ version: "v1.2.0", sha: "b".repeat(40) },
];
const provider = fakeProvider({ "actions/checkout": tags, "actions/setup-node": tags });

function workflow(ref: string): string {
	return `jobs:\n  build:\n    steps:\n      - uses: actions/checkout@${ref}\n`;
}

function hoverAt(document: FakeDocument, line: number, character: number): Promise<vscode.Hover | undefined> {
	return new ActupHoverProvider(provider, checkResultsByUri).provideHover(
		document,
		new vscode.Position(line, character),
	);
}

function rows(hover: vscode.Hover | undefined): string[] {
	return (hover?.contents ?? []).flatMap(content => (typeof content === "string" ? content : content.value).split("\n"));
}

async function statusRows(document: FakeDocument): Promise<string[]> {
	return rows(await hoverAt(document, 3, 20)).filter(row => row.startsWith("$("));
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

	async function computeSameCommit(ref: string, track: "major" | "exact"): Promise<FakeDocument> {
		const twins = fakeProvider({
			"actions/checkout": [
				{ version: "v1", sha: "e".repeat(40) },
				{ version: "v1.0.5", sha: "e".repeat(40) },
			],
		});
		const document = new FakeDocument(`hover-same-commit-${ref}.yml`, workflow(ref));
		const real = { ...wasm };
		await mock.module("actup-wasm", () => ({
			...real,
			checkRefs: (text: string, p: wasm.ActupProvider) => real.checkRefs(text, p, { policy: { track } }),
		}));
		try {
			await computeDiagnostics(document.uri, document.getText(), twins, createDiagnosticCollection());
		} finally {
			await mock.module("actup-wasm", () => real);
		}
		return document;
	}

	test("shows a pin of the same commit like the LSP", async () => {
		const document = await computeSameCommit("v1", "exact");

		expect(await statusRows(document)).toEqual(["$(info) Pin available, same commit: tag v1 = v1.0.5 (eeeeeee)"]);
	});

	test("offers no pin for a less specific target on the same commit", async () => {
		const document = await computeSameCommit("v1.0.5", "major");

		expect(await statusRows(document)).toEqual(["$(info) Same commit: tag v1.0.5 = v1 (eeeeeee)"]);
	});

	test("shows no status for a ref edited since the last computation", async () => {
		const document = new FakeDocument("hover-edited.yml", workflow("v1.0.0"));
		await computeDiagnostics(document.uri, document.getText(), provider, createDiagnosticCollection());
		const edited = new FakeDocument("hover-edited.yml", workflow("v1.2.0"));

		expect(await statusRows(edited)).toEqual([]);
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

describe("hover range", () => {
	test("covers a quoted value without its quotes", async () => {
		const document = new FakeDocument("hover-quoted.yml", "jobs:\n  build:\n    steps:\n      - uses: 'actions/checkout@v1.0.0'\n");

		expect((await hoverAt(document, 3, 20))?.range).toEqual(rangeOf(document, 3, "actions/checkout@v1.0.0"));
	});

	test("covers the quoted scalar when the line holds the value escaped", async () => {
		const document = new FakeDocument(
			"hover-escaped.yml",
			"jobs:\n  build:\n    steps:\n      - uses: \"actions/checkout@v1.0.\\x30\"\n",
		);
		await computeDiagnostics(document.uri, document.getText(), provider, createDiagnosticCollection());
		const hover = await hoverAt(document, 3, 20);

		expect(hover?.range).toEqual(rangeOf(document, 3, "\"actions/checkout@v1.0.\\x30\""));
		expect(rows(hover)).toContain("$(warning) Outdated: `v1.0.0` → `v1.2.0`");
	});

	test("describes the escaped value under the cursor on a line with two escaped values", async () => {
		const line = "    steps: [{ uses: \"actions/checkout@v1.0.\\x30\" }, { uses: \"actions/setup-node@v1.2.\\x30\" }]";
		const document = new FakeDocument("hover-two-escaped.yml", `jobs:\n  build:\n${line}\n`);
		await computeDiagnostics(document.uri, document.getText(), provider, createDiagnosticCollection());
		const first = await hoverAt(document, 2, line.indexOf("checkout"));
		const second = await hoverAt(document, 2, line.indexOf("setup-node"));

		expect(first?.range).toEqual(rangeOf(document, 2, "\"actions/checkout@v1.0.\\x30\""));
		expect(rows(first)).toEqual(expect.arrayContaining(["**actions/checkout**", "$(warning) Outdated: `v1.0.0` → `v1.2.0`"]));
		expect(second?.range).toEqual(rangeOf(document, 2, "\"actions/setup-node@v1.2.\\x30\""));
		expect(rows(second)).toEqual(expect.arrayContaining(["**actions/setup-node**", "$(check) Up to date at `v1.2.0`"]));
		expect(await hoverAt(document, 2, line.indexOf("steps"))).toBeUndefined();
		expect(await hoverAt(document, 2, line.indexOf("}, {"))).toBeUndefined();
	});

	test("describes the action under the cursor on a line with two actions", async () => {
		const line = "    steps: [{ uses: actions/checkout@v1.0.0 }, { uses: actions/setup-node@v1.2.0 }]";
		const document = new FakeDocument("hover-two.yml", `jobs:\n  build:\n${line}\n`);
		await computeDiagnostics(document.uri, document.getText(), provider, createDiagnosticCollection());
		const first = await hoverAt(document, 2, line.indexOf("checkout"));
		const second = await hoverAt(document, 2, line.indexOf("setup-node"));

		expect(first?.range).toEqual(rangeOf(document, 2, "actions/checkout@v1.0.0"));
		expect(rows(first)).toEqual(expect.arrayContaining(["**actions/checkout**", "$(warning) Outdated: `v1.0.0` → `v1.2.0`"]));
		expect(second?.range).toEqual(rangeOf(document, 2, "actions/setup-node@v1.2.0"));
		expect(rows(second)).toEqual(expect.arrayContaining(["**actions/setup-node**", "$(check) Up to date at `v1.2.0`"]));
	});

	test("shows nothing for a local action or a docker image", async () => {
		const document = new FakeDocument(
			"hover-unparsed.yml",
			"jobs:\n  build:\n    steps:\n      - uses: ./local/action\n      - uses: docker://alpine:3\n",
		);

		expect(await hoverAt(document, 3, 16)).toBeUndefined();
		expect(await hoverAt(document, 4, 16)).toBeUndefined();
	});

	test("shows nothing when the cursor is outside every value on the line", async () => {
		const line = "    steps: [{ uses: actions/checkout@v1.0.0 }, { uses: actions/setup-node@v1.2.0 }]";
		const document = new FakeDocument("hover-between.yml", `jobs:\n  build:\n${line}\n`);

		expect(await hoverAt(document, 2, line.indexOf("}, {"))).toBeUndefined();
	});
});

describe("hover repository link", () => {
	async function linkRow(value: string): Promise<string | undefined> {
		const document = new FakeDocument("hover-link.yml", `jobs:\n  build:\n    steps:\n      - uses: ${value}\n`);
		return rows(await hoverAt(document, 3, 16)).find(row => row.startsWith("[View on "));
	}

	test("links a bare ref to the default host", async () => {
		expect(await linkRow("actions/checkout@v1")).toBe("[View on github.com](https://github.com/actions/checkout)");
	});

	test("links a GHE ref to its host", async () => {
		expect(await linkRow("ghe.corp.internal/o/r@v1")).toBe("[View on ghe.corp.internal](https://ghe.corp.internal/o/r)");
	});

	test("links a GitLab ref to GitLab", async () => {
		expect(await linkRow("gitlab.com/group/project@v1")).toBe("[View on gitlab.com](https://gitlab.com/group/project)");
	});
});
