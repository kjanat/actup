import { parseConfig, type RepoConfig, repoConfigCandidates } from "actup-wasm";
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

function parsed(path: string, contents: string): FolderConfig {
	try {
		return { kind: "loaded", path, config: parseConfig(path, contents) };
	} catch (err) {
		return { kind: "invalid", path, message: err instanceof Error ? err.message : String(err) };
	}
}

/** The file that `configPath` names for `folder`: absolute, or relative to `folder`. */
export function resolveConfigPath(folder: vscode.Uri, configPath: string): vscode.Uri {
	return configPath.startsWith("/") ? folder.with({ path: configPath }) : vscode.Uri.joinPath(folder, configPath);
}

/**
 * Load the repo config of `folder`.
 *
 * An explicit `configPath`, relative to `folder` or absolute, must exist.
 * Without one, the first of core's repo config candidates in `folder` wins.
 */
export async function loadFolderConfig(folder: vscode.Uri, configPath: string | undefined): Promise<FolderConfig> {
	if (configPath !== undefined) {
		let contents: string;
		try {
			contents = await readText(resolveConfigPath(folder, configPath));
		} catch (err) {
			const reason = err instanceof Error ? err.message : String(err);
			return { kind: "invalid", path: configPath, message: `cannot read ${configPath}: ${reason}` };
		}
		return parsed(configPath, contents);
	}
	for (const candidate of repoConfigCandidates()) {
		let contents: string;
		try {
			contents = await readText(vscode.Uri.joinPath(folder, candidate));
		} catch {
			continue;
		}
		return parsed(candidate, contents);
	}
	return { kind: "none" };
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
		explicit = configPath === undefined ? [] : (vscode.workspace.workspaceFolders ?? []).flatMap((folder) => {
			const file = resolveConfigPath(folder.uri, configPath);
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
