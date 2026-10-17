import { parseUsesRefs } from "actup-wasm";
import type { ActupProvider, LocatedCheckResult, UsesLocation } from "actup-wasm";
import * as vscode from "vscode";
import { byteToColumn } from "./position.ts";

export class ActupHoverProvider implements vscode.HoverProvider {
	constructor(
		private readonly provider: ActupProvider,
		private readonly resultCache: ReadonlyMap<string, readonly LocatedCheckResult[]>,
	) {}

	async provideHover(
		document: vscode.TextDocument,
		position: vscode.Position,
	): Promise<vscode.Hover | undefined> {
		const text = document.getText();
		const onLine = parseUsesRefs(text).filter(l => l.line - 1 === position.line);
		const loc = onLine.find(l => {
			const columns = valueColumns(l);
			return columns !== undefined && position.character >= columns.start && position.character < columns.end;
		}) ?? onLine.find(l => l.value_range === null);
		if (loc === undefined) {
			return undefined;
		}

		const ref = parseActionRef(loc.value);
		if (ref === undefined) {
			return undefined;
		}

		const results = this.resultCache.get(document.uri.toString());
		const matchingResult = results?.find(r => r.location.line === loc.line && r.location.column === loc.column);

		// isTrusted=false: action.yml content is remote and untrusted; trusted Markdown
		// would let it execute commands or render unsafe links.
		const md = new vscode.MarkdownString();
		md.isTrusted = false;

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
		const columns = valueColumns(loc) ?? { start: 0, end: loc.raw_line.length };
		return new vscode.Hover(md, new vscode.Range(line, columns.start, line, columns.end));
	}
}

function valueColumns(loc: UsesLocation): { start: number; end: number } | undefined {
	const range = loc.value_range;
	return range === null
		? undefined
		: { start: byteToColumn(loc.raw_line, range.start), end: byteToColumn(loc.raw_line, range.end) };
}

function parseActionRef(
	value: string,
): { owner: string; repo: string; gitRef: string; host: string | null; subpath: string | null } | undefined {
	const atIdx = value.lastIndexOf("@");
	if (atIdx === -1) return undefined;

	const path = value.slice(0, atIdx);
	const gitRef = value.slice(atIdx + 1);
	const parts = path.split("/").filter(p => p !== "");

	if (parts.length < 2) return undefined;

	// Host detection: first segment with `.` is treated as a host (e.g. `github.com`).
	const first = parts[0] ?? "";
	let host: string | null = null;
	let owner: string;
	let repo: string;
	let subpathParts: string[];
	if (first.includes(".") && parts.length >= 3) {
		host = first;
		owner = parts[1] ?? "";
		repo = parts[2] ?? "";
		subpathParts = parts.slice(3);
	} else {
		owner = first;
		repo = parts[1] ?? "";
		subpathParts = parts.slice(2);
	}

	return {
		owner,
		repo,
		gitRef,
		host,
		subpath: subpathParts.length > 0 ? subpathParts.join("/") : null,
	};
}
