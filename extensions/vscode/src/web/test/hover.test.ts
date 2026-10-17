import * as wasm from "actup-wasm";
import { describe, expect, mock, test } from "bun:test";
import * as vscode from "vscode";
import { checkResultsByUri, clearDiagnosticData, computeDiagnostics, createDiagnosticCollection } from "../diagnostics.ts";
import { ActupHoverProvider } from "../hover.ts";
import { FakeDocument, fakeProvider, hoverStatusRows, rangeOf } from "./fixtures.ts";

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
	return hoverStatusRows(await hoverAt(document, 3, 20));
}

describe("hover status from computed diagnostics", () => {
	test("shows the status that computeDiagnostics stored", async () => {
		const document = new FakeDocument("hover-status.yml", workflow("v1.0.0"));
		await computeDiagnostics(document.uri, document.getText(), provider, createDiagnosticCollection());

		expect(await statusRows(document)).toEqual(["**Outdated**: v1.0.0 -> v1.2.0 (`bbbbbbb`)"]);
	});

	test("shows the status of the latest computation", async () => {
		const collection = createDiagnosticCollection();
		const stale = new FakeDocument("hover-recompute.yml", workflow("v1.0.0"));
		await computeDiagnostics(stale.uri, stale.getText(), provider, collection);
		const current = new FakeDocument("hover-recompute.yml", workflow("v1.2.0"));
		await computeDiagnostics(current.uri, current.getText(), provider, collection);

		expect(await statusRows(current)).toEqual(["Up to date"]);
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

		expect(await statusRows(document)).toEqual(["**Pin available**, same commit: tag `v1` = v1.0.5 (`eeeeeee`)"]);
	});

	test("offers no pin for a less specific target on the same commit", async () => {
		const document = await computeSameCommit("v1.0.5", "major");

		expect(await statusRows(document)).toEqual(["**Same commit**: tag `v1.0.5` = v1 (`eeeeeee`)"]);
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

describe("hover status like the LSP hover", () => {
	const head = "d".repeat(40);
	const branchProvider: wasm.ActupProvider = {
		...provider,
		getVersions: () => Promise.resolve({ semverTags: tags, defaultBranch: { name: "main", sha: head } }),
	};

	async function computed(name: string, ref: string, using: wasm.ActupProvider): Promise<FakeDocument> {
		const document = new FakeDocument(name, workflow(ref));
		await computeDiagnostics(document.uri, document.getText(), using, createDiagnosticCollection());
		return document;
	}

	async function hoverRows(name: string, ref: string, policy = "", using = branchProvider): Promise<string[]> {
		const document = new FakeDocument(name, workflow(ref));
		const config = wasm.parseConfig("actup.toml", policy);
		await computeDiagnostics(
			document.uri,
			document.getText(),
			using,
			createDiagnosticCollection(),
			() => Promise.resolve({ kind: "loaded", path: "actup.toml", config }),
		);
		return rows(await hoverAt(document, 3, 20));
	}

	test("names the tag of a SHA pin", async () => {
		expect(await hoverRows("hover-pinned.yml", "b".repeat(40))).toContain("**Pinned** to `bbbbbbbbbbbb` (v1.2.0)");
	});

	test("names an unknown version for a SHA pin without a tag", async () => {
		expect(await hoverRows("hover-pinned-unknown.yml", "9".repeat(40))).toContain(
			"**Pinned** to `999999999999` (unknown version)",
		);
	});

	test("names a floating branch under policy.branch = warn", async () => {
		expect(await hoverRows("hover-floating.yml", "main", "[policy]\nbranch = \"warn\"\n")).toContain(
			"**Floating branch** `main` — pin to a tag or SHA",
		);
	});

	test("names the pin of a branch under policy.branch = update", async () => {
		expect(await hoverRows("hover-pin-branch.yml", "main", "[policy]\nbranch = \"update\"\n")).toContain(
			"**Pin branch** `main` -> `dddddddddddd` # main",
		);
	});

	test("names a ref the version list lacks as a floating ref under policy.branch = warn", async () => {
		expect(await hoverRows("hover-floating-ref.yml", "stable", "[policy]\nbranch = \"warn\"\n")).toContain(
			"**Floating ref** `stable` — pin to a tag or SHA",
		);
	});

	test("names the pin of a ref the version list lacks under policy.branch = update", async () => {
		expect(await hoverRows("hover-pin-ref.yml", "stable", "[policy]\nbranch = \"update\"\n")).toContain(
			"**Pin ref** `stable` -> `aaaaaaaaaaaa` # stable",
		);
	});

	test("names an unresolvable ref", async () => {
		const missing: wasm.ActupProvider = {
			...provider,
			getVersions: () => Promise.reject(Object.assign(new Error("no repository"), { kind: "notFound" })),
		};
		expect(hoverStatusRows(await hoverAt(await computed("hover-unresolvable.yml", "v1.0.0", missing), 3, 20)))
			.toEqual(["**Unresolvable**: action not found: github.com/actions/checkout"]);
	});

	test("names a check that failed on the network", async () => {
		const offline: wasm.ActupProvider = {
			...provider,
			getVersions: () => Promise.reject(Object.assign(new Error("offline"), { kind: "network" })),
		};
		expect(hoverStatusRows(await hoverAt(await computed("hover-offline.yml", "v1.0.0", offline), 3, 20)))
			.toEqual(["Check failed: network error during js provider call: offline"]);
	});

	test("names an ignored ref", async () => {
		expect(await hoverRows("hover-ignored.yml", "v1.0.0", "[scan]\nignore = [\"actions/checkout\"]\n")).toContain(
			"Ignored by config",
		);
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
		expect(rows(hover)).toContain("**Outdated**: v1.0.0 -> v1.2.0 (`bbbbbbb`)");
	});

	test("describes the escaped value under the cursor on a line with two escaped values", async () => {
		const line = "    steps: [{ uses: \"actions/checkout@v1.0.\\x30\" }, { uses: \"actions/setup-node@v1.2.\\x30\" }]";
		const document = new FakeDocument("hover-two-escaped.yml", `jobs:\n  build:\n${line}\n`);
		await computeDiagnostics(document.uri, document.getText(), provider, createDiagnosticCollection());
		const first = await hoverAt(document, 2, line.indexOf("checkout"));
		const second = await hoverAt(document, 2, line.indexOf("setup-node"));

		expect(first?.range).toEqual(rangeOf(document, 2, "\"actions/checkout@v1.0.\\x30\""));
		expect(rows(first)).toEqual(expect.arrayContaining(["**actions/checkout**", "**Outdated**: v1.0.0 -> v1.2.0 (`bbbbbbb`)"]));
		expect(second?.range).toEqual(rangeOf(document, 2, "\"actions/setup-node@v1.2.\\x30\""));
		expect(rows(second)).toEqual(expect.arrayContaining(["**actions/setup-node**", "Up to date"]));
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
		expect(rows(first)).toEqual(expect.arrayContaining(["**actions/checkout**", "**Outdated**: v1.0.0 -> v1.2.0 (`bbbbbbb`)"]));
		expect(second?.range).toEqual(rangeOf(document, 2, "actions/setup-node@v1.2.0"));
		expect(rows(second)).toEqual(expect.arrayContaining(["**actions/setup-node**", "Up to date"]));
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
