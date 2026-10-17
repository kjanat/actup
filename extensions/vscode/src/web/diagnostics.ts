import type { ActupProvider, UpdateEdit, UsesLocation } from "actup-wasm";
import { checkRefs, type CheckStatus, type LocatedCheckResult, planUpdates } from "actup-wasm";
import * as vscode from "vscode";
import { type ConfigLookup, noConfig } from "./config.ts";
import { byteToColumn } from "./position.ts";

const DIAGNOSTIC_SOURCE = "actup";

/** Stored edit for an outdated or pin-branch diagnostic, keyed by document URI string. */
export type DiagnosticData =
	| {
		readonly kind: "outdated";
		readonly targetRef: string;
		readonly range: vscode.Range;
		readonly newText: string;
	}
	| {
		readonly kind: "pin-branch";
		readonly sha: string;
		readonly refName: string;
		readonly range: vscode.Range;
		readonly newText: string;
	};

/** Per-document diagnostic results for code actions to consume. */
export const diagnosticDataByUri = new Map<string, Map<string, DiagnosticData>>();

/** Per-document check results for the hover to consume. */
export const checkResultsByUri = new Map<string, LocatedCheckResult[]>();

const latestComputeByUri = new Map<string, number>();
let computeCount = 0;

/** Create a `DiagnosticCollection` for actup. */
export function createDiagnosticCollection(): vscode.DiagnosticCollection {
	return vscode.languages.createDiagnosticCollection("actup");
}

/**
 * Compute diagnostics for a document under its repo config and publish them.
 *
 * An invalid repo config publishes no diagnostics.
 */
export async function computeDiagnostics(
	uri: vscode.Uri,
	text: string,
	provider: ActupProvider,
	collection: vscode.DiagnosticCollection,
	configs: ConfigLookup = noConfig,
): Promise<void> {
	const key = uri.toString();
	const compute = ++computeCount;
	latestComputeByUri.set(key, compute);

	if (!text.includes("uses:")) {
		collection.set(uri, []);
		clearDiagnosticData(uri);
		return;
	}

	let checked: { results: LocatedCheckResult[]; edits: UpdateEdit[] } | undefined;
	try {
		const folderConfig = await configs(uri);
		if (folderConfig.kind === "invalid") {
			throw new Error(folderConfig.message);
		}
		const config = folderConfig.kind === "loaded" ? folderConfig.config : undefined;
		const results = await checkRefs(text, provider, config);
		checked = { results, edits: planUpdates(results, config) };
	} catch {
		checked = undefined;
	}

	if (latestComputeByUri.get(key) !== compute) {
		return;
	}
	if (checked === undefined) {
		// Network/provider error — clear diagnostics rather than show stale data
		collection.set(uri, []);
		clearDiagnosticData(uri);
		return;
	}

	const { results, edits } = checked;
	const editByRef = new Map(edits.map(edit => [`${edit.line}:${edit.refStart}`, edit]));
	const diagnostics: vscode.Diagnostic[] = [];
	const dataMap = new Map<string, DiagnosticData>();

	for (const result of results) {
		const refStart = result.location.ref_range?.start;
		const edit = refStart === undefined ? undefined : editByRef.get(`${result.location.line}:${refStart}`);
		const diag = statusToDiagnostic(result, edit);
		if (diag === undefined) {
			continue;
		}

		const diagnostic = sameCommitDiagnostic(diag.diagnostic, result);
		diagnostics.push(diagnostic);
		if (diag.data !== undefined) {
			dataMap.set(diagnosticKey(diagnostic), diag.data);
		}
	}

	collection.set(uri, diagnostics);
	diagnosticDataByUri.set(key, dataMap);
	checkResultsByUri.set(key, results);
}

/** Clear diagnostic data and check results when a document closes. */
export function clearDiagnosticData(uri: vscode.Uri): void {
	const key = uri.toString();
	latestComputeByUri.delete(key);
	diagnosticDataByUri.delete(key);
	checkResultsByUri.delete(key);
}

/** Unique key for a diagnostic (for data lookup). */
export function diagnosticKey(d: vscode.Diagnostic): string {
	return `${d.range.start.line}:${d.range.start.character}:${d.range.end.character}`;
}

/** Name the ref and the target on the commit the ref already names, when the result has one. */
export function sameCommitDetail(result: LocatedCheckResult): string | undefined {
	const target = result.same_commit;
	if (target === null) {
		return undefined;
	}
	const kind = result.ref_kind === null ? "" : `${result.ref_kind} `;
	return `${kind}${result.action_ref.ref_str} = ${target.version} (${target.sha.slice(0, 7)})`;
}

function sameCommitDiagnostic(diag: vscode.Diagnostic, result: LocatedCheckResult): vscode.Diagnostic {
	const detail = sameCommitDetail(result);
	if (detail === undefined) {
		return diag;
	}
	const lead = result.same_commit_pins ? "pin available, " : "";
	const sameCommit = new vscode.Diagnostic(
		diag.range,
		`${result.location.value}: ${lead}same commit: ${detail}`,
		vscode.DiagnosticSeverity.Information,
	);
	sameCommit.source = DIAGNOSTIC_SOURCE;
	sameCommit.code = "same-commit";
	return sameCommit;
}

interface DiagResult {
	diagnostic: vscode.Diagnostic;
	data?: DiagnosticData;
}

function editRange(loc: UsesLocation, edit: UpdateEdit): vscode.Range {
	const line = loc.line - 1;
	return new vscode.Range(
		line,
		byteToColumn(loc.raw_line, edit.refStart),
		line,
		byteToColumn(loc.raw_line, edit.replaceEnd),
	);
}

function statusToDiagnostic(result: LocatedCheckResult, edit: UpdateEdit | undefined): DiagResult | undefined {
	const status: CheckStatus = result.status;
	const loc = result.location;
	// UpToDate — no diagnostic
	if (status === "UpToDate") return undefined;

	const line = loc.line - 1; // 1-indexed -> 0-indexed
	const refRange = loc.ref_range;
	const range = refRange !== null
		? new vscode.Range(line, byteToColumn(loc.raw_line, refRange.start), line, byteToColumn(loc.raw_line, refRange.end))
		: new vscode.Range(line, 0, line, loc.raw_line.length);
	const kind = result.ref_kind ?? "ref";

	if (typeof status === "object" && "Outdated" in status) {
		const { target, current } = status.Outdated;
		const targetRef = target.version;
		const shortSha = target.sha?.slice(0, 7) ?? "";
		const shaInfo = shortSha !== "" ? ` (${shortSha})` : "";

		const diag = new vscode.Diagnostic(
			range,
			`${loc.value}: ${current} -> ${targetRef}${shaInfo}`,
			vscode.DiagnosticSeverity.Warning,
		);
		diag.source = DIAGNOSTIC_SOURCE;
		diag.code = "outdated";
		if (edit === undefined) {
			return { diagnostic: diag };
		}
		return {
			diagnostic: diag,
			data: { kind: "outdated", targetRef, range: editRange(loc, edit), newText: edit.newText },
		};
	}

	if (typeof status === "object" && "Pinned" in status) {
		const { sha, tag } = status.Pinned;
		const tagInfo = tag === null ? "unknown version" : `= ${tag}`;
		const diag = new vscode.Diagnostic(
			range,
			`${loc.value}: pinned to ${sha.slice(0, 7)} (${tagInfo})`,
			vscode.DiagnosticSeverity.Hint,
		);
		diag.source = DIAGNOSTIC_SOURCE;
		diag.code = "pinned";
		return { diagnostic: diag };
	}

	if (typeof status === "object" && "Floating" in status) {
		const diag = new vscode.Diagnostic(
			range,
			`${loc.value}: '${status.Floating.ref_name}' is a floating ${kind}, pin to a tag or SHA`,
			vscode.DiagnosticSeverity.Warning,
		);
		diag.source = DIAGNOSTIC_SOURCE;
		diag.code = "floating";
		return { diagnostic: diag };
	}

	if (typeof status === "object" && "PinBranch" in status) {
		const { ref_name: refName, sha } = status.PinBranch;
		const diag = new vscode.Diagnostic(
			range,
			`${loc.value}: pin ${kind} '${refName}' to ${sha.slice(0, 7)} # ${refName}`,
			vscode.DiagnosticSeverity.Warning,
		);
		diag.source = DIAGNOSTIC_SOURCE;
		diag.code = "pin-branch";
		if (edit === undefined) {
			return { diagnostic: diag };
		}
		return {
			diagnostic: diag,
			data: { kind: "pin-branch", sha, refName, range: editRange(loc, edit), newText: edit.newText },
		};
	}

	if (typeof status === "object" && "Unresolvable" in status) {
		const { reason, is_environment_error } = status.Unresolvable;
		const severity = is_environment_error
			? vscode.DiagnosticSeverity.Information
			: vscode.DiagnosticSeverity.Error;
		const message = is_environment_error
			? `${loc.value}: check failed (${reason})`
			: `${loc.value}: ${reason}`;

		const diag = new vscode.Diagnostic(range, message, severity);
		diag.source = DIAGNOSTIC_SOURCE;
		diag.code = "unresolvable";
		return { diagnostic: diag };
	}

	return undefined;
}
