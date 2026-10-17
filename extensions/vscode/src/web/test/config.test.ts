import { type ActupProvider, checkRefs, parseConfig } from "actup-wasm";
import { beforeEach, describe, expect, test } from "bun:test";
import * as vscode from "vscode";
import { ConfigStore, type FolderConfig, loadFolderConfig, watchConfig } from "../config.ts";
import { computeDiagnostics, createDiagnosticCollection } from "../diagnostics.ts";
import { fixAllInWorkspace } from "../extension.ts";
import { FakeDocument, FakeLogOutputChannel, fakeProvider, rangeOf } from "./fixtures.ts";
import { Uri as MockUri, window as mockWindow, workspace as mockWorkspace } from "./vscode.ts";

const folder = vscode.Uri.parse("file:///repo");

function file(path: string, text: string): void {
	mockWorkspace.fs.files.set(`file:///repo/${path}`, text);
}

function addFolder(): void {
	mockWorkspace.folders.push({ uri: MockUri.parse("file:///repo"), name: "repo", index: 0 });
}

function kindOf(config: FolderConfig): string {
	return config.kind === "none" ? "none" : `${config.kind} ${config.path}`;
}

beforeEach(() => {
	mockWorkspace.reset();
	mockWindow.reset();
});

describe("loadFolderConfig", () => {
	test("takes the first repo config candidate in the folder", async () => {
		file(".config/actup.toml", "[policy]\nbump = \"patch\"\n");
		file("actup.toml", "[policy]\nbump = \"minor\"\n");

		const config = await loadFolderConfig(folder, undefined);

		expect(kindOf(config)).toBe("loaded actup.toml");
		expect(config.kind === "loaded" ? config.config.policy?.bump : undefined).toBe("minor");
	});

	test("finds a candidate below .config and answers none without one", async () => {
		expect(await loadFolderConfig(folder, undefined)).toEqual({ kind: "none" });
		file(".config/actup.yaml", "policy:\n  bump: patch\n");

		expect(kindOf(await loadFolderConfig(folder, undefined))).toBe("loaded .config/actup.yaml");
	});

	test("reports an invalid file with core's message", async () => {
		file("actup.toml", "[policy]\ntracking = 1\n");

		const config = await loadFolderConfig(folder, undefined);

		expect(config.kind).toBe("invalid");
		expect(config.kind === "invalid" ? config.message : "").toMatch(/actup\.toml.*tracking/s);
	});

	test("applies an explicit path, relative or absolute, over the first candidate", async () => {
		file("actup.toml", "[policy]\nbump = \"minor\"\n");
		file("ci/actup.json", "{ \"default_host\": \"ghe.corp.example\", \"policy\": { \"bump\": \"patch\" } }");

		const relative = await loadFolderConfig(folder, "ci/actup.json");
		const absolute = await loadFolderConfig(folder, "/repo/ci/actup.json");

		expect(kindOf(relative)).toBe("loaded ci/actup.json");
		expect(relative.kind === "loaded" ? relative.config.policy?.bump : undefined).toBe("patch");
		expect(relative.kind === "loaded" ? relative.config.default_host : undefined).toBe("ghe.corp.example");
		expect(kindOf(absolute)).toBe("loaded /repo/ci/actup.json");
	});

	test("drops default_host from a discovered candidate and keeps it from the explicit file", async () => {
		file("actup.toml", "default_host = \"ghe.corp.example\"\n");

		const discovered = await loadFolderConfig(folder, undefined);
		const explicit = await loadFolderConfig(folder, "actup.toml");

		expect(discovered.kind === "loaded" ? discovered.config.default_host : "unloaded").toBeNull();
		expect(explicit.kind === "loaded" ? explicit.config.default_host : "unloaded").toBe("ghe.corp.example");
	});

	test("keeps the candidate's overrides under the explicit file", async () => {
		file("actup.toml", "[policy]\nbump = \"minor\"\n\n[[overrides]]\npattern = \"actions/*\"\npolicy = { pin = \"sha\" }\n");
		file("ci/actup.toml", "default_host = \"ghe.corp.example\"\n");

		const config = await loadFolderConfig(folder, "ci/actup.toml");

		expect(kindOf(config)).toBe("loaded ci/actup.toml");
		const loaded = config.kind === "loaded" ? config.config : undefined;
		expect(loaded?.default_host).toBe("ghe.corp.example");
		expect(loaded?.policy?.bump).toBe("minor");
		expect(loaded?.["overrides"]).toMatchObject([{ pattern: "actions/*", policy: { pin: "sha" } }]);
		expect(mockWorkspace.fs.reads).toEqual(["file:///repo/actup.toml", "file:///repo/ci/actup.toml"]);
	});

	test("reads the candidate once when the explicit path names it", async () => {
		file("actup.toml", "default_host = \"ghe.corp.example\"\n");

		for (const configPath of ["actup.toml", "./actup.toml", "ci/../actup.toml", "/repo/actup.toml"]) {
			mockWorkspace.fs.reads.splice(0);
			const config = await loadFolderConfig(folder, configPath);

			expect(kindOf(config)).toBe(`loaded ${configPath}`);
			expect(config.kind === "loaded" ? config.config.default_host : undefined).toBe("ghe.corp.example");
			expect(mockWorkspace.fs.reads).toEqual(["file:///repo/actup.toml"]);
		}
	});

	test("reports an invalid candidate under the explicit file", async () => {
		file("actup.toml", "[policy]\ntracking = 1\n");
		file("ci/actup.toml", "default_host = \"ghe.corp.example\"\n");

		const config = await loadFolderConfig(folder, "ci/actup.toml");

		expect(kindOf(config)).toBe("invalid ci/actup.toml");
		expect(config.kind === "invalid" ? config.message : "").toMatch(/actup\.toml.*tracking/s);
	});

	test("reports a missing explicit path as invalid", async () => {
		file("actup.toml", "[policy]\nbump = \"minor\"\n");

		const config = await loadFolderConfig(folder, "missing.toml");

		expect(kindOf(config)).toBe("invalid missing.toml");
		expect(config.kind === "invalid" ? config.message : "").toStartWith("cannot read missing.toml: FileNotFound");
	});
});

describe("ConfigStore", () => {
	test("loads each folder once, honours actup.configPath and reloads after clear", async () => {
		addFolder();
		file("actup.toml", "[policy]\nbump = \"minor\"\n");
		file("other.toml", "[policy]\nbump = \"patch\"\n");
		const store = new ConfigStore(() => {});
		const document = vscode.Uri.parse("file:///repo/.github/workflows/ci.yml");

		expect(kindOf(await store.configFor(document))).toBe("loaded actup.toml");
		expect(kindOf(await store.configFor(document))).toBe("loaded actup.toml");
		expect(mockWorkspace.fs.reads).toEqual(["file:///repo/actup.toml"]);

		mockWorkspace.settings.set("actup.configPath", "other.toml");
		expect(kindOf(await store.configFor(document))).toBe("loaded actup.toml");
		store.clear();
		expect(kindOf(await store.configFor(document))).toBe("loaded other.toml");

		mockWorkspace.settings.set("actup.configPath", "  ");
		store.clear();
		expect(kindOf(await store.configFor(document))).toBe("loaded actup.toml");
	});

	test("answers none outside every folder and reports an invalid config once", async () => {
		addFolder();
		file("actup.toml", "update_policy = \"patch\"\n");
		const invalid: string[] = [];
		const store = new ConfigStore((path, message) => invalid.push(`${path}: ${message}`));

		expect(await store.configFor(vscode.Uri.parse("file:///elsewhere/ci.yml"))).toEqual({ kind: "none" });
		await store.configFor(vscode.Uri.parse("file:///repo/a.yml"));
		await store.configFor(vscode.Uri.parse("file:///repo/b.yml"));

		expect(invalid).toHaveLength(1);
		expect(invalid[0]).toMatch(/^actup\.toml: .*update_policy/);
	});
});

describe("watchConfig", () => {
	test("reloads on the setting, on folder changes and on candidate file events", () => {
		let reloads = 0;
		const disposables = watchConfig(new ConfigStore(() => {}), () => reloads++);
		const [watcher] = mockWorkspace.watchers;
		if (watcher === undefined) {
			throw new Error("no watcher");
		}

		expect(mockWorkspace.watchers).toHaveLength(1);
		expect(watcher.glob).toStartWith("**/{actup.toml,actup.json,");
		expect(watcher.glob).toContain(".config/actup.yml");
		mockWorkspace.configurationChanges.fire({ affectsConfiguration: section => section === "actup.configPath" });
		mockWorkspace.configurationChanges.fire({ affectsConfiguration: section => section === "actup.server" });
		mockWorkspace.folderChanges.fire(undefined);
		expect(reloads).toBe(2);

		watcher.created.fire(MockUri.parse("file:///repo/actup.toml"));
		watcher.changed.fire(MockUri.parse("file:///repo/actup.toml"));
		watcher.deleted.fire(MockUri.parse("file:///repo/actup.toml"));
		expect(reloads).toBe(5);

		for (const disposable of disposables) {
			disposable.dispose();
		}
		expect(watcher.disposed).toBe(true);
		expect(mockWorkspace.configurationChanges.size).toBe(0);
		expect(mockWorkspace.folderChanges.size).toBe(0);
	});

	test("watches the file that actup.configPath names for each folder, outside the folder too", () => {
		addFolder();
		mockWorkspace.folders.push({ uri: MockUri.parse("file:///other"), name: "other", index: 1 });
		mockWorkspace.settings.set("actup.configPath", "../shared/actup.toml");
		let reloads = 0;
		const disposables = watchConfig(new ConfigStore(() => {}), () => reloads++);
		const explicit = () => mockWorkspace.watchers.flatMap(watcher => typeof watcher.glob === "string" || watcher.disposed ? [] : [watcher]);
		const targets = () =>
			explicit().map(watcher => typeof watcher.glob === "string" ? "" : `${watcher.glob.base.toString()} ${watcher.glob.pattern}`);

		expect(targets()).toEqual(["file:///shared actup.toml"]);
		explicit()[0]?.changed.fire(MockUri.parse("file:///shared/actup.toml"));
		explicit()[0]?.deleted.fire(MockUri.parse("file:///shared/actup.toml"));
		expect(reloads).toBe(2);

		mockWorkspace.settings.set("actup.configPath", "/cfg/actup.json");
		mockWorkspace.configurationChanges.fire({ affectsConfiguration: section => section === "actup.configPath" });
		expect(targets()).toEqual(["file:///cfg actup.json"]);
		expect(reloads).toBe(3);

		mockWorkspace.settings.set("actup.configPath", "ci/settings.toml");
		mockWorkspace.folderChanges.fire(undefined);
		expect(targets()).toEqual(["file:///repo/ci settings.toml", "file:///other/ci settings.toml"]);

		mockWorkspace.settings.set("actup.configPath", "ci/actup.json");
		mockWorkspace.folderChanges.fire(undefined);
		expect(targets()).toEqual([]);

		mockWorkspace.folders.splice(1);
		mockWorkspace.settings.set("actup.configPath", "/cfg/actup.json");
		mockWorkspace.folderChanges.fire(undefined);
		expect(targets()).toEqual(["file:///cfg actup.json"]);

		mockWorkspace.settings.set("actup.configPath", null);
		mockWorkspace.configurationChanges.fire({ affectsConfiguration: section => section === "actup.configPath" });
		expect(targets()).toEqual([]);

		mockWorkspace.settings.set("actup.configPath", "ci/settings.toml");
		mockWorkspace.folderChanges.fire(undefined);
		expect(targets()).toEqual(["file:///repo/ci settings.toml"]);
		for (const disposable of disposables) {
			disposable.dispose();
		}
		expect(targets()).toEqual([]);
	});
});

const provider = fakeProvider({
	"actions/checkout": [
		{ version: "v1.0.0", sha: "a".repeat(40) },
		{ version: "v1.2.0", sha: "b".repeat(40) },
		{ version: "v2.0.0", sha: "c".repeat(40) },
	],
});

const workflow = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v1.0.0\n";

describe("the repo config in diagnostics and fix-all", () => {
	test("reaches computeDiagnostics through policy, overrides and scan.ignore", async () => {
		addFolder();
		const store = new ConfigStore(() => {});
		const diagnose = async (name: string): Promise<string[]> => {
			const document = new FakeDocument(`repo/.github/workflows/${name}`, workflow);
			await computeDiagnostics(document.uri, workflow, provider, createDiagnosticCollection(), store.configFor);
			return vscode.languages.getDiagnostics(document.uri).map(diagnostic => diagnostic.message);
		};

		expect(await diagnose("default.yml")).toEqual([`actions/checkout@v1.0.0: v1.0.0 -> v2.0.0 (${"c".repeat(7)})`]);
		file("actup.toml", "[[overrides]]\npattern = \"actions/*\"\npolicy = { bump = \"minor\" }\n");
		store.clear();
		expect(await diagnose("minor.yml")).toEqual([`actions/checkout@v1.0.0: v1.0.0 -> v1.2.0 (${"b".repeat(7)})`]);
		file("actup.toml", "[scan]\nignore = [\"actions/checkout\"]\n");
		store.clear();
		expect(await diagnose("ignored.yml")).toEqual([]);
		file("actup.toml", "[policy]\ntracking = 1\n");
		store.clear();
		expect(await diagnose("invalid.yml")).toEqual([]);
	});

	test("reaches fix-all, which pins a branch ref under policy.branch = update", async () => {
		addFolder();
		file("actup.toml", "[policy]\nbranch = \"update\"\n");
		const head = "d".repeat(40);
		const branchProvider: ActupProvider = {
			...provider,
			getVersions: () =>
				Promise.resolve({ semverTags: [{ version: "v1.0.0", sha: "a".repeat(40) }], defaultBranch: { name: "main", sha: head } }),
		};
		const text = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@main\n";
		const document = new FakeDocument("repo/.github/workflows/branch.yml", text);
		mockWorkspace.documents.push(document);
		const store = new ConfigStore(() => {});

		const [result] = await checkRefs(text, branchProvider, parseConfig("actup.toml", "[policy]\nbranch = \"update\"\n"));
		await fixAllInWorkspace(branchProvider, new FakeLogOutputChannel(), store.configFor);

		expect(result?.status).toEqual({ PinBranch: { ref_name: "main", sha: head } });
		const line = document.lineAt(3).text;
		const pinned = mockWorkspace.appliedEdits
			.flatMap(edit => edit.entries().flatMap(([, edits]) => edits))
			.map(edit => line.slice(0, edit.range.start.character) + edit.newText + line.slice(edit.range.end.character));
		expect(pinned).toEqual([`      - uses: actions/checkout@${head} # main`]);
		expect(mockWindow.messages).toEqual(["actup: Updated 1 action ref(s) across 1 file(s)."]);
	});

	test("reaches fix-all, which pins through an override and skips a file under an invalid config", async () => {
		addFolder();
		file("actup.toml", "[[overrides]]\npattern = \"actions/*\"\npolicy = { pin = \"sha\" }\n");
		const document = new FakeDocument("repo/.github/workflows/ci.yml", workflow);
		mockWorkspace.documents.push(document);
		const store = new ConfigStore(() => {});

		await fixAllInWorkspace(provider, new FakeLogOutputChannel(), store.configFor);

		expect(mockWorkspace.appliedEdits.flatMap(edit => edit.entries().flatMap(([, edits]) => edits))).toEqual([
			new vscode.TextEdit(rangeOf(document, 3, "v1.0.0"), `${"c".repeat(40)} # v2.0.0`),
		]);

		mockWorkspace.appliedEdits.splice(0);
		mockWindow.reset();
		file("actup.toml", "[policy]\ntracking = 1\n");
		store.clear();
		const log = new FakeLogOutputChannel();
		await fixAllInWorkspace(provider, log, store.configFor);

		expect(mockWorkspace.appliedEdits).toEqual([]);
		expect(mockWindow.messages).toEqual(["actup: No updates applied (1 file(s) failed; see actup output)."]);
		expect(log.lines.join("\n")).toMatch(/invalid config actup\.toml: .*tracking/s);
	});
});
