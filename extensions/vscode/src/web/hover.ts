import { parseUses, parseUsesRefs } from "actup-wasm";
import type { ActionRef, ActupProvider, LocatedCheckResult, UsesLocation } from "actup-wasm";
import * as vscode from "vscode";
import { byteToColumn } from "./position.ts";

const DEFAULT_HOST = "github.com";

export class ActupHoverProvider implements vscode.HoverProvider {
	constructor(
		private readonly provider: ActupProvider,
		private readonly resultCache: ReadonlyMap<string, readonly LocatedCheckResult[]>,
	) {}

	async provideHover(
		document: vscode.TextDocument,
		position: vscode.Position,
	): Promise<vscode.Hover | undefined> {
		const hit = parseUsesRefs(document.getText()).flatMap(loc => {
			const columns = loc.line - 1 === position.line ? hoverColumns(loc) : undefined;
			return columns !== undefined && position.character >= columns.start && position.character < columns.end
				? [{ loc, columns }]
				: [];
		})[0];
		if (hit === undefined) {
			return undefined;
		}
		const { loc, columns } = hit;

		const ref = parseRef(loc.value);
		if (ref === undefined) {
			return undefined;
		}

		const host = ref.host ?? DEFAULT_HOST;
		const results = this.resultCache.get(document.uri.toString());
		const matchingResult = results?.find(r =>
			r.location.line === loc.line && r.location.column === loc.column && r.location.value === loc.value
		);

		// isTrusted=false: action.yml content is remote and untrusted; trusted Markdown
		// would let it execute commands or render unsafe links.
		const md = new vscode.MarkdownString();
		md.isTrusted = false;

		// Header
		md.appendMarkdown(`**${ref.owner}/${ref.repo}**\n\n`);

		// Try to fetch action metadata for description
		try {
			const actionYml = await this.provider.getActionYml(
				host,
				ref.owner,
				ref.repo,
				ref.ref_str,
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
			const sameCommit = matchingResult.same_commit;
			if (sameCommit !== null) {
				const kind = matchingResult.ref_kind === null ? "" : `${matchingResult.ref_kind} `;
				const lead = matchingResult.same_commit_pins ? "**Pin available**, same commit" : "**Same commit**";
				md.appendMarkdown(
					`${lead}: ${kind}\`${ref.ref_str}\` = ${sameCommit.version} (\`${sameCommit.sha.slice(0, 7)}\`)\n`,
				);
			} else if (status === "UpToDate") {
				md.appendMarkdown("Up to date\n");
			} else if (typeof status === "object" && "Outdated" in status) {
				const { current, target } = status.Outdated;
				md.appendMarkdown(`**Outdated**: ${current} -> ${target.version} (\`${target.sha.slice(0, 7)}\`)\n`);
			} else if (typeof status === "object" && "Unresolvable" in status) {
				const { reason, is_environment_error } = status.Unresolvable;
				md.appendMarkdown(is_environment_error ? `Check failed: ${reason}\n` : `**Unresolvable**: ${reason}\n`);
			} else if (status === "Ignored") {
				md.appendMarkdown("Ignored by config\n");
			} else if (typeof status === "object" && "Pinned" in status) {
				const { sha, tag } = status.Pinned;
				md.appendMarkdown(`**Pinned** to \`${sha.slice(0, 12)}\` (${tag ?? "unknown version"})\n`);
			} else if (typeof status === "object" && "Floating" in status) {
				const kind = matchingResult.ref_kind ?? "ref";
				md.appendMarkdown(`**Floating ${kind}** \`${status.Floating.ref_name}\` — pin to a tag or SHA\n`);
			} else if (typeof status === "object" && "PinBranch" in status) {
				const kind = matchingResult.ref_kind ?? "ref";
				const { ref_name: refName, sha } = status.PinBranch;
				md.appendMarkdown(`**Pin ${kind}** \`${refName}\` -> \`${sha.slice(0, 12)}\` # ${refName}\n`);
			}
		}

		md.appendMarkdown(`\n[View on ${host}](https://${host}/${ref.owner}/${ref.repo})`);

		const line = loc.line - 1;
		return new vscode.Hover(md, new vscode.Range(line, columns.start, line, columns.end));
	}
}

function hoverColumns(loc: UsesLocation): { start: number; end: number } | undefined {
	const range = loc.value_range ?? loc.scalar_range;
	return range === null
		? undefined
		: { start: byteToColumn(loc.raw_line, range.start), end: byteToColumn(loc.raw_line, range.end) };
}

function parseRef(value: string): ActionRef | undefined {
	try {
		return parseUses(value) ?? undefined;
	} catch {
		return undefined;
	}
}
