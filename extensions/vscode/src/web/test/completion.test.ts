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

async function versionItems(value: string): Promise<vscode.CompletionItem[]> {
	const line = `      - uses: ${value}`;
	const document = new FakeDocument("completion.yml", `jobs:\n  b:\n    steps:\n${line}\n`);
	const list = await completion.provideCompletionItems(
		document,
		new vscode.Position(3, line.length),
		cancellationToken,
		invokeCompletion,
	);
	return list?.items ?? [];
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
