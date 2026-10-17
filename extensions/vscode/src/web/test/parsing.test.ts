import type { ActupProvider } from "actup-wasm";
import { describe, expect, test } from "bun:test";
import * as vscode from "vscode";
import { ActupCompletionProvider } from "../completion.ts";
import { ActupHoverProvider } from "../hover.ts";
import { cancellationToken, FakeDocument, FakeLogOutputChannel, fakeProvider, invokeCompletion } from "./fixtures.ts";

function recordingProvider(): { calls: string[][]; provider: ActupProvider } {
	const calls: string[][] = [];
	const base = fakeProvider({});
	return {
		calls,
		provider: {
			...base,
			getVersions: (host, owner, repo) => {
				calls.push([host, owner, repo]);
				return base.getVersions(host, owner, repo);
			},
			getActionYml: (host, owner, repo, ref) => {
				calls.push([host, owner, repo, ref]);
				return base.getActionYml(host, owner, repo, ref);
			},
		},
	};
}

function documentWith(value: string): { document: FakeDocument; line: string } {
	const line = `      - uses: ${value}`;
	return { document: new FakeDocument("parsing.yml", `jobs:\n  b:\n    steps:\n${line}\n`), line };
}

async function hover(value: string): Promise<{ header: string | undefined; calls: string[][] }> {
	const { calls, provider } = recordingProvider();
	const { document } = documentWith(value);
	const shown = await new ActupHoverProvider(provider, new Map()).provideHover(document, new vscode.Position(3, 15));
	const first = shown?.contents[0];
	const text = typeof first === "string" ? first : first?.value;
	return { header: text?.split("\n")[0], calls };
}

async function complete(value: string): Promise<{ items: vscode.CompletionItem[]; calls: string[][] }> {
	const { calls, provider } = recordingProvider();
	const { document, line } = documentWith(value);
	const list = await new ActupCompletionProvider(provider, new FakeLogOutputChannel()).provideCompletionItems(
		document,
		new vscode.Position(3, line.length),
		cancellationToken,
		invokeCompletion,
	);
	return { items: list?.items ?? [], calls };
}

describe("web hover parses refs with core", () => {
	test("a dotted first segment of a two-segment path is the owner", async () => {
		expect(await hover("a.b/c@v1")).toEqual({ header: "**a.b/c**", calls: [["github.com", "a.b", "c", "v1"]] });
	});

	test("a host is lower-cased", async () => {
		expect((await hover("GitHub.COM/actions/checkout@v1")).calls).toEqual([["github.com", "actions", "checkout", "v1"]]);
	});

	test("`localhost`, ports and IPv6 literals are hosts", async () => {
		expect((await hover("localhost/o/r@v1")).calls).toEqual([["localhost", "o", "r", "v1"]]);
		expect((await hover("ghe.corp:8443/o/r@v1")).calls).toEqual([["ghe.corp:8443", "o", "r", "v1"]]);
		expect((await hover("\"[::1]/o/r@v1\"")).calls).toEqual([["[::1]", "o", "r", "v1"]]);
	});

	test("a first segment that is no DNS name is the owner", async () => {
		expect(await hover("-bad.host/r/sub@v1")).toEqual({
			header: "**-bad.host/r**",
			calls: [["github.com", "-bad.host", "r", "v1"]],
		});
	});

	test("an invalid owner gets no hover", async () => {
		expect(await hover("bad!owner/r@v1")).toEqual({ header: undefined, calls: [] });
	});
});

describe("web completion parses refs with core", () => {
	test("version lookups use core's owner, repo and host", async () => {
		expect((await complete("a.b/c@")).calls).toEqual([["github.com", "a.b", "c"]]);
		expect((await complete("GitHub.COM/actions/checkout@")).calls).toEqual([["github.com", "actions", "checkout"]]);
		expect((await complete("\"localhost:8080/o/r/sub@")).calls).toEqual([["localhost:8080", "o", "r"]]);
	});

	test("a path core rejects looks up no versions", async () => {
		expect(await complete("./local@")).toEqual({ items: [], calls: [] });
		expect(await complete("bad!owner/r@")).toEqual({ items: [], calls: [] });
	});

	test("name completion keeps a host prefix only when core names a host", async () => {
		const host = await complete("localhost/actions/check");
		const owner = await complete("-bad.host/act");

		expect(host.items.map(item => item.insertText)).toEqual(["localhost/actions/checkout@v6"]);
		expect(owner.items).toEqual([]);
	});
});
