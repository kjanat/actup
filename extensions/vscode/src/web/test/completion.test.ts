import { describe, expect, test } from "bun:test";
import * as vscode from "vscode";
import { ActupCompletionProvider } from "../completion.ts";
import { cancellationToken, FakeDocument, FakeLogOutputChannel, fakeProvider, invokeCompletion } from "./fixtures.ts";

const apiOrder = [
	"v1.0.0",
	"v1.1.0",
	"v1.2.0",
	"v1.3.0",
	"v1.4.0",
	"v1.5.0",
	"v1.6.0",
	"v1.7.0",
	"v1.8.0",
	"v1.9.0",
	"v1.10.0",
	"v2.0.0-rc.1",
	"v2.0.0",
];

const completion = new ActupCompletionProvider(
	fakeProvider({ "actions/checkout": apiOrder.map(version => ({ version, sha: "a".repeat(40) })) }),
	new FakeLogOutputChannel(),
);

async function itemsAtEnd(line: string): Promise<vscode.CompletionItem[] | undefined> {
	const document = new FakeDocument("completion.yml", `jobs:\n  b:\n    steps:\n${line}\n`);
	const list = await completion.provideCompletionItems(
		document,
		new vscode.Position(3, line.length),
		cancellationToken,
		invokeCompletion,
	);
	return list?.items;
}

async function versionItems(value: string): Promise<vscode.CompletionItem[]> {
	return (await itemsAtEnd(`      - uses: ${value}`)) ?? [];
}

describe("ActupCompletionProvider version completion", () => {
	test("lists tags newest first with index sort text", async () => {
		const items = await versionItems("actions/checkout@");

		expect(items.map(item => item.label)).toEqual([
			"v2.0.0",
			"v2.0.0-rc.1",
			"v1.10.0",
			"v1.9.0",
			"v1.8.0",
			"v1.7.0",
			"v1.6.0",
			"v1.5.0",
			"v1.4.0",
			"v1.3.0",
			"v1.2.0",
			"v1.1.0",
			"v1.0.0",
		]);
		expect(items.map(item => item.sortText)).toEqual(items.map((_, index) => String(index).padStart(2, "0")));
	});

	test("keeps semver order inside a typed prefix", async () => {
		const items = await versionItems("actions/checkout@v1.1");

		expect(items.map(item => item.label)).toEqual(["v1.10.0", "v1.1.0"]);
		expect(items.map(item => item.sortText)).toEqual(["0", "1"]);
	});
});

describe("ActupCompletionProvider value start", () => {
	const allActions = [
		"actions/checkout",
		"actions/setup-node",
		"actions/setup-python",
		"actions/cache",
		"actions/upload-artifact",
		"actions/download-artifact",
		"actions/setup-go",
		"actions/setup-java",
		"actions/github-script",
		"actions/setup-dotnet",
		"docker/build-push-action",
		"docker/login-action",
		"docker/setup-buildx-action",
	];

	test("an empty value lists every action and replaces from the value start", async () => {
		const items = (await itemsAtEnd("      - uses: ")) ?? [];

		expect(items.map(item => item.label)).toEqual(allActions);
		expect(items.map(item => item.range)).toEqual(items.map(() => new vscode.Range(3, 14, 3, 14)));
	});

	test("a value whose first character occurs earlier in the line filters by the value alone", async () => {
		const items = (await itemsAtEnd("      - uses: u")) ?? [];

		expect(items.map(item => item.label)).toEqual(allActions.filter(label => label.includes("u")));
		expect(items[0]?.range).toEqual(new vscode.Range(3, 14, 3, 15));
	});

	test("a flow-style step completes action names", async () => {
		const line = "      - { name: x, uses: actions/ch";
		const items = (await itemsAtEnd(line)) ?? [];

		expect(items.map(item => item.label)).toEqual(["actions/checkout"]);
		expect(items[0]?.range).toEqual(new vscode.Range(3, line.indexOf("actions/"), 3, line.length));
	});

	test("a flow-style step completes versions", async () => {
		const items = (await itemsAtEnd("    steps: [{ uses: actions/checkout@v1.1")) ?? [];

		expect(items.map(item => item.label)).toEqual(["v1.10.0", "v1.1.0"]);
	});

	test("the cursor past the end of a flow value gets no completion", async () => {
		expect(await itemsAtEnd("      - { uses: actions/checkout@v1, with: ")).toBeUndefined();
	});

	test("`uses:` inside another value gets no completion", async () => {
		expect(await itemsAtEnd("      - run: echo uses: act")).toBeUndefined();
	});
});
