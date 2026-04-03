import { parseUsesRefs } from "actup-wasm";
import type { ActupProvider, CheckResult } from "actup-wasm";
import * as vscode from "vscode";

export class ActupHoverProvider implements vscode.HoverProvider {
	constructor(
		private readonly provider: ActupProvider,
		private readonly resultCache: Map<string, CheckResult[]>,
	) {}

	async provideHover(
		document: vscode.TextDocument,
		position: vscode.Position,
	): Promise<vscode.Hover | undefined> {
		const text = document.getText();
		const locations = parseUsesRefs(text);

		// Find the uses: ref at the cursor position
		const loc = locations.find(l => {
			const line = l.line - 1; // 1-indexed -> 0-indexed
			return line === position.line;
		});
		if (loc === undefined) return undefined;

		const ref = parseActionRef(loc.value);
		if (ref === undefined) return undefined;

		const results = this.resultCache.get(document.uri.toString());
		const matchingResult = results?.find(
			r => r.action_ref.owner === ref.owner && r.action_ref.repo === ref.repo,
		);

		const md = new vscode.MarkdownString();
		md.isTrusted = true;

		// Header
		md.appendMarkdown(`**${ref.owner}/${ref.repo}**\n\n`);

		// Try to fetch action metadata for description
		try {
			const actionYml = await this.provider.getActionYml(
				ref.host ?? "github.com",
				ref.owner,
				ref.repo,
				ref.gitRef,
			);
			if (actionYml.name !== null) {
				md.appendMarkdown(`*${actionYml.name}*\n\n`);
			}
			if (actionYml.description !== null) {
				md.appendMarkdown(`${actionYml.description}\n\n`);
			}
		} catch {
			// action.yml fetch failed — continue with version info
		}

		// Version status
		if (matchingResult !== undefined) {
			const status = matchingResult.status;
			if (status === "UpToDate") {
				md.appendMarkdown(`$(check) Up to date at \`${ref.gitRef}\`\n`);
			} else if (typeof status === "object" && "Outdated" in status) {
				const target = status.Outdated.target.version;
				md.appendMarkdown(`$(warning) Outdated: \`${ref.gitRef}\` → \`${target}\`\n`);
			} else if (typeof status === "object" && "Unresolvable" in status) {
				md.appendMarkdown(`$(error) ${status.Unresolvable.reason}\n`);
			}
		}

		md.appendMarkdown(
			`\n[View on GitHub](https://github.com/${ref.owner}/${ref.repo})`,
		);

		const line = loc.line - 1;
		const range = new vscode.Range(line, loc.column - 1, line, loc.column - 1 + loc.value.length);
		return new vscode.Hover(md, range);
	}
}

function parseActionRef(value: string): { owner: string; repo: string; gitRef: string; host: string | null } | undefined {
	const atIdx = value.lastIndexOf("@");
	if (atIdx === -1) return undefined;

	const path = value.slice(0, atIdx);
	const gitRef = value.slice(atIdx + 1);
	const parts = path.split("/");

	if (parts.length < 2) return undefined;

	return {
		owner: parts[0] ?? "",
		repo: parts[1] ?? "",
		gitRef,
		host: null,
	};
}
