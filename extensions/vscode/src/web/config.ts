import { type ConfigFile, parseConfig, parseRepoAndExplicitConfig, type RepoConfig, repoConfigCandidates } from "actup-wasm";
import * as vscode from "vscode";
import { CONFIG_PATH_SETTING, configPathSetting } from "../settings.ts";

/** The repo config of a workspace folder. */
export type FolderConfig =
	| { readonly kind: "none" }
	| { readonly kind: "loaded"; readonly path: string; readonly config: RepoConfig }
	| { readonly kind: "invalid"; readonly path: string; readonly message: string };

/** Find the repo config of the folder holding a document. */
export type ConfigLookup = (uri: vscode.Uri) => Promise<FolderConfig>;

/** A lookup for documents outside any configured folder. */
export const noConfig: ConfigLookup = () => Promise.resolve({ kind: "none" });

async function readText(uri: vscode.Uri): Promise<string> {
	return new TextDecoder().decode(await vscode.workspace.fs.readFile(uri));
}

function parsed(path: string, parse: () => RepoConfig): FolderConfig {
	try {
		return { kind: "loaded", path, config: parse() };
	} catch (err) {
		return { kind: "invalid", path, message: err instanceof Error ? err.message : String(err) };
	}
}

/** The file that `configPath` names for `folder`: absolute, or relative to `folder`. */
export function resolveConfigPath(folder: vscode.Uri, configPath: string): vscode.Uri {
	return vscode.Uri.joinPath(configPath.startsWith("/") ? folder.with({ path: "/" }) : folder, configPath);
}

interface Candidate extends ConfigFile {
	readonly uri: vscode.Uri;
}

async function firstCandidate(folder: vscode.Uri): Promise<Candidate | undefined> {
	for (const path of repoConfigCandidates()) {
		const uri = vscode.Uri.joinPath(folder, path);
		try {
			return { path, contents: await readText(uri), uri };
		} catch {
			continue;
		}
	}
	return undefined;
}

/**
 * Load the repo config of `folder`.
 *
 * The first of core's repo config candidates in `folder` applies. An explicit
 * `configPath`, relative to `folder` or absolute, must exist, and every key
 * of it applies over the candidate.
 */
export async function loadFolderConfig(folder: vscode.Uri, configPath: string | undefined): Promise<FolderConfig> {
	const candidate = await firstCandidate(folder);
	if (configPath === undefined) {
		return candidate === undefined
			? { kind: "none" }
			: parsed(candidate.path, () => parseConfig(candidate.path, candidate.contents));
	}
	const target = resolveConfigPath(folder, configPath);
	const repo = candidate?.uri.toString() === target.toString() ? undefined : candidate;
	let contents: string;
	if (candidate !== undefined && repo === undefined) {
		contents = candidate.contents;
	} else {
		try {
			contents = await readText(target);
		} catch (err) {
			const reason = err instanceof Error ? err.message : String(err);
			return { kind: "invalid", path: configPath, message: `cannot read ${configPath}: ${reason}` };
		}
	}
	const repoFile = repo === undefined ? undefined : { path: repo.path, contents: repo.contents };
	return parsed(configPath, () => parseRepoAndExplicitConfig(repoFile, { path: configPath, contents }));
}

/** Caches the repo config of each workspace folder until a reload. */
export class ConfigStore {
	private readonly byFolder = new Map<string, Promise<FolderConfig>>();

	constructor(private readonly onInvalid: (path: string, message: string) => void) {}

	/** The repo config of the workspace folder holding `uri`. */
	readonly configFor: ConfigLookup = (uri) => {
		const folder = vscode.workspace.getWorkspaceFolder(uri);
		if (folder === undefined) {
			return Promise.resolve({ kind: "none" });
		}
		const key = folder.uri.toString();
		const cached = this.byFolder.get(key);
		if (cached !== undefined) {
			return cached;
		}
		const loading = loadFolderConfig(folder.uri, configPathSetting()).then((config) => {
			if (config.kind === "invalid") {
				this.onInvalid(config.path, config.message);
			}
			return config;
		});
		this.byFolder.set(key, loading);
		return loading;
	};

	/** Forget every loaded config. */
	clear(): void {
		this.byFolder.clear();
	}
}

/**
 * Clear `store` and call `reload` when `actup.configPath` or the workspace
 * folders change, when a repo config candidate is created, changed or
 * deleted, and when the file that `actup.configPath` names for a folder is.
 */
export function watchConfig(store: ConfigStore, reload: () => void): vscode.Disposable[] {
	const changed = (): void => {
		store.clear();
		reload();
	};
	const watch = (watcher: vscode.FileSystemWatcher): vscode.Disposable[] => [
		watcher.onDidCreate(changed),
		watcher.onDidChange(changed),
		watcher.onDidDelete(changed),
		watcher,
	];
	let explicit: vscode.Disposable[] = [];
	const watchExplicit = (): void => {
		for (const disposable of explicit) {
			disposable.dispose();
		}
		const configPath = configPathSetting();
		const targets = configPath === undefined
			? []
			: (vscode.workspace.workspaceFolders ?? []).map(folder => resolveConfigPath(folder.uri, configPath));
		const files = new Map<string, vscode.Uri>();
		for (const file of targets) {
			const covered = vscode.workspace.getWorkspaceFolder(file) !== undefined
				&& repoConfigCandidates().some(candidate => file.path.endsWith(`/${candidate}`));
			if (!covered) {
				files.set(file.toString(), file);
			}
		}
		explicit = [...files.values()].flatMap((file) => {
			const name = file.path.slice(file.path.lastIndexOf("/") + 1);
			const pattern = new vscode.RelativePattern(vscode.Uri.joinPath(file, ".."), name);
			return watch(vscode.workspace.createFileSystemWatcher(pattern));
		});
	};
	watchExplicit();
	return [
		vscode.workspace.onDidChangeConfiguration((e) => {
			if (e.affectsConfiguration(CONFIG_PATH_SETTING)) {
				watchExplicit();
				changed();
			}
		}),
		vscode.workspace.onDidChangeWorkspaceFolders(() => {
			watchExplicit();
			changed();
		}),
		...watch(vscode.workspace.createFileSystemWatcher(`**/{${repoConfigCandidates().join(",")}}`)),
		{
			dispose: () => {
				for (const disposable of explicit) {
					disposable.dispose();
				}
			},
		},
	];
}
