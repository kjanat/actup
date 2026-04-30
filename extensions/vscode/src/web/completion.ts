import type { ActupProvider } from "actup-wasm";
import * as vscode from "vscode";

/** Popular actions for name completion, matching the LSP's list. */
const POPULAR_ACTIONS: readonly {
	readonly owner: string;
	readonly repo: string;
	readonly description: string;
	readonly defaultRef: string;
}[] = [
	{ owner: "actions", repo: "checkout", description: "Check out a repository", defaultRef: "v6" },
	{ owner: "actions", repo: "setup-node", description: "Set up Node.js environment", defaultRef: "v6" },
	{ owner: "actions", repo: "setup-python", description: "Set up Python environment", defaultRef: "v6" },
	{ owner: "actions", repo: "cache", description: "Cache dependencies and build outputs", defaultRef: "v5" },
	{ owner: "actions", repo: "upload-artifact", description: "Upload build artifacts", defaultRef: "v7" },
	{ owner: "actions", repo: "download-artifact", description: "Download build artifacts", defaultRef: "v8" },
	{ owner: "actions", repo: "setup-go", description: "Set up Go environment", defaultRef: "v6" },
	{ owner: "actions", repo: "setup-java", description: "Set up Java JDK environment", defaultRef: "v5" },
	{ owner: "actions", repo: "github-script", description: "Run JavaScript in workflow with GitHub API", defaultRef: "v9" },
	{ owner: "actions", repo: "setup-dotnet", description: "Set up .NET SDK environment", defaultRef: "v5" },
	{ owner: "docker", repo: "build-push-action", description: "Build and push Docker images", defaultRef: "v7" },
	{ owner: "docker", repo: "login-action", description: "Log in to Docker registry", defaultRef: "v4" },
	{ owner: "docker", repo: "setup-buildx-action", description: "Set up Docker Buildx", defaultRef: "v4" },
];

export class ActupCompletionProvider implements vscode.CompletionItemProvider {
	constructor(private readonly provider: ActupProvider) {}

	async provideCompletionItems(
		document: vscode.TextDocument,
		position: vscode.Position,
		_token: vscode.CancellationToken,
		_context: vscode.CompletionContext,
	): Promise<vscode.CompletionList | undefined> {
		const line = document.lineAt(position.line).text;

		// Only complete on `uses:` lines
		const usesMatch = /^\s*-?\s*uses:\s*(.*)$/.exec(line);
		if (usesMatch === null) {
			return undefined;
		}

		const valueStart = line.indexOf(usesMatch[1] ?? "");
		const typed = line.slice(valueStart, position.character);

		// After @: suggest version tags
		const atIdx = typed.lastIndexOf("@");
		if (atIdx !== -1) {
			return this.completeVersions(typed.slice(0, atIdx), typed.slice(atIdx + 1));
		}

		// Before @: suggest action names
		return this.completeActionNames(typed);
	}

	private completeActionNames(prefix: string): vscode.CompletionList {
		const lower = prefix.toLowerCase();
		const items: vscode.CompletionItem[] = [];

		for (const action of POPULAR_ACTIONS) {
			const label = `${action.owner}/${action.repo}`;
			if (lower !== "" && !label.toLowerCase().includes(lower)) continue;

			const item = new vscode.CompletionItem(label, vscode.CompletionItemKind.Reference);
			item.detail = action.description;
			item.insertText = `${label}@${action.defaultRef}`;
			items.push(item);
		}

		return new vscode.CompletionList(items, false);
	}

	private async completeVersions(
		actionPath: string,
		versionPrefix: string,
	): Promise<vscode.CompletionList> {
		const parsed = parseActionPath(actionPath);
		if (parsed === undefined) return new vscode.CompletionList([], false);

		const items: vscode.CompletionItem[] = [];

		try {
			const versions = await this.provider.getVersions(parsed.host, parsed.owner, parsed.repo);
			const lower = versionPrefix.toLowerCase();

			for (const tag of versions.semverTags) {
				if (lower !== "" && !tag.version.toLowerCase().startsWith(lower)) continue;

				const item = new vscode.CompletionItem(
					tag.version,
					vscode.CompletionItemKind.Value,
				);
				item.detail = tag.sha.slice(0, 7);
				items.push(item);
			}
		} catch {
			// Provider error — return empty completions
		}

		return new vscode.CompletionList(items, false);
	}
}

/** Parse `[host/]owner/repo[/subpath]`. Skips local refs (`./...`) and quoted forms. */
function parseActionPath(path: string): { host: string; owner: string; repo: string } | undefined {
	let trimmed = path.trim();
	if (trimmed === "" || trimmed.startsWith("./") || trimmed.startsWith("../")) return undefined;
	if (trimmed.startsWith("\"") || trimmed.startsWith("'")) {
		trimmed = trimmed.slice(1);
	}
	const parts = trimmed.split("/").filter(p => p !== "");
	if (parts.length < 2) return undefined;

	// Host detection: first segment contains a `.` (e.g. `github.com`, `ghe.corp.internal`).
	const first = parts[0] ?? "";
	if (first.includes(".") && parts.length >= 3) {
		return { host: first, owner: parts[1] ?? "", repo: parts[2] ?? "" };
	}
	return { host: "github.com", owner: first, repo: parts[1] ?? "" };
}
