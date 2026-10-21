import type { ActupProvider } from 'actup-wasm';
import { GitHubProviderError, isHostSegment, parseActionPath, sortVersionTags } from 'actup-wasm';
import * as vscode from 'vscode';

/** Popular actions for name completion, matching the LSP's list. */
const POPULAR_ACTIONS: readonly {
	readonly owner: string;
	readonly repo: string;
	readonly description: string;
	readonly defaultRef: string;
}[] = [
	{ owner: 'actions', repo: 'checkout', description: 'Check out a repository', defaultRef: 'v6' },
	{ owner: 'actions', repo: 'setup-node', description: 'Set up Node.js environment', defaultRef: 'v6' },
	{ owner: 'actions', repo: 'setup-python', description: 'Set up Python environment', defaultRef: 'v6' },
	{ owner: 'actions', repo: 'cache', description: 'Cache dependencies and build outputs', defaultRef: 'v5' },
	{ owner: 'actions', repo: 'upload-artifact', description: 'Upload build artifacts', defaultRef: 'v7' },
	{ owner: 'actions', repo: 'download-artifact', description: 'Download build artifacts', defaultRef: 'v8' },
	{ owner: 'actions', repo: 'setup-go', description: 'Set up Go environment', defaultRef: 'v6' },
	{ owner: 'actions', repo: 'setup-java', description: 'Set up Java JDK environment', defaultRef: 'v5' },
	{ owner: 'actions', repo: 'github-script', description: 'Run JavaScript in workflow with GitHub API', defaultRef: 'v9' },
	{ owner: 'actions', repo: 'setup-dotnet', description: 'Set up .NET SDK environment', defaultRef: 'v5' },
	{ owner: 'docker', repo: 'build-push-action', description: 'Build and push Docker images', defaultRef: 'v7' },
	{ owner: 'docker', repo: 'login-action', description: 'Log in to Docker registry', defaultRef: 'v4' },
	{ owner: 'docker', repo: 'setup-buildx-action', description: 'Set up Docker Buildx', defaultRef: 'v4' },
];

const USES_KEY = /(?:^\s*(?:-\s+)?|[{,]\s*)uses:[ \t]*/g;

export class ActupCompletionProvider implements vscode.CompletionItemProvider {
	constructor(
		private readonly provider: ActupProvider,
		private readonly outputChannel: vscode.LogOutputChannel,
	) {}

	async provideCompletionItems(
		document: vscode.TextDocument,
		position: vscode.Position,
		_token: vscode.CancellationToken,
		_context: vscode.CompletionContext,
	): Promise<vscode.CompletionList | undefined> {
		const beforeCursor = document.lineAt(position.line).text.slice(0, position.character);
		const usesKey = [...beforeCursor.matchAll(USES_KEY)].at(-1);
		if (usesKey === undefined) {
			return undefined;
		}

		const valueStart = usesKey.index + usesKey[0].length;
		const typed = beforeCursor.slice(valueStart);
		if (/[\s,{}[\]]/.test(typed)) {
			return undefined;
		}

		// After @: suggest version tags
		const atIdx = typed.lastIndexOf('@');
		if (atIdx !== -1) {
			return this.completeVersions(typed.slice(0, atIdx), typed.slice(atIdx + 1));
		}

		// Before @: suggest action names. Pass the full typed range so the
		// completion can deterministically replace whatever the user has typed
		// (including any leading quote / host prefix).
		const replaceRange = new vscode.Range(
			position.line,
			valueStart,
			position.line,
			position.character,
		);
		return this.completeActionNames(typed, replaceRange);
	}

	private completeActionNames(prefix: string, range: vscode.Range): vscode.CompletionList {
		let stripped = prefix;
		let quoteChar = '';
		if (stripped.startsWith('"') || stripped.startsWith("'")) {
			quoteChar = stripped.slice(0, 1);
			stripped = stripped.slice(1);
		}
		let hostPrefix = '';
		const slashIdx = stripped.indexOf('/');
		if (slashIdx > 0 && isHostSegment(stripped.slice(0, slashIdx))) {
			hostPrefix = stripped.slice(0, slashIdx + 1);
			stripped = stripped.slice(slashIdx + 1);
		}

		const lower = stripped.toLowerCase();
		const items: vscode.CompletionItem[] = [];

		for (const action of POPULAR_ACTIONS) {
			const label = `${action.owner}/${action.repo}`;
			if (lower !== '' && !label.toLowerCase().includes(lower)) continue;

			const item = new vscode.CompletionItem(label, vscode.CompletionItemKind.Reference);
			item.detail = action.description;
			item.insertText = `${quoteChar}${hostPrefix}${label}@${action.defaultRef}`;
			item.range = range;
			items.push(item);
		}

		return new vscode.CompletionList(items, false);
	}

	private async completeVersions(
		actionPath: string,
		versionPrefix: string,
	): Promise<vscode.CompletionList> {
		const parsed = repositoryOf(actionPath);
		if (parsed === undefined) return new vscode.CompletionList([], false);

		const items: vscode.CompletionItem[] = [];

		try {
			const versions = await this.provider.getVersions(parsed.host, parsed.owner, parsed.repo);
			const lower = versionPrefix.toLowerCase();
			const tags = sortVersionTags(versions.semverTags ?? [])
				.filter(tag => tag.version.toLowerCase().startsWith(lower));
			const width = String(tags.length).length;

			for (const [index, tag] of tags.entries()) {
				const item = new vscode.CompletionItem(
					tag.version,
					vscode.CompletionItemKind.Value,
				);
				item.detail = tag.sha.slice(0, 7);
				item.sortText = String(index).padStart(width, '0');
				items.push(item);
			}
		} catch (err) {
			const msg = err instanceof Error ? err.message : String(err);
			this.outputChannel.warn(`actup completion: ${msg}`);
			if (err instanceof Error && err.stack !== undefined) {
				this.outputChannel.debug(err.stack);
			}

			if (err instanceof GitHubProviderError) {
				if (err.status === 429 || err.retryAfter !== undefined) {
					this.outputChannel.warn('actup: rate limited — completions unavailable, retry later');
				} else if (err.status === 401) {
					this.outputChannel.warn('actup: auth failure — set GITHUB_TOKEN for version completions');
				} else if (err.status === 403) {
					this.outputChannel.warn('actup: forbidden — check token scopes');
				}
			} else if (err instanceof TypeError && /fetch|network/i.test(msg)) {
				this.outputChannel.warn('actup: network error — check connectivity');
			}
		}

		return new vscode.CompletionList(items, false);
	}
}

function repositoryOf(path: string): { host: string; owner: string; repo: string } | undefined {
	const trimmed = path.trim();
	const unquoted = trimmed.startsWith('"') || trimmed.startsWith("'") ? trimmed.slice(1) : trimmed;
	try {
		const id = parseActionPath(unquoted);
		return { host: id.host ?? 'github.com', owner: id.owner, repo: id.repo };
	} catch {
		return undefined;
	}
}
