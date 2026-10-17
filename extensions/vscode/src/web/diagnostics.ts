import type { ActupProvider, UpdateEdit, UsesLocation } from "actup-wasm";
import { checkRefs, type CheckStatus, type LocatedCheckResult, planUpdates } from "actup-wasm";
import * as vscode from "vscode";
import { byteToColumn } from "./position.ts";

const DIAGNOSTIC_SOURCE = "actup";

/** Stored edit for an outdated diagnostic, keyed by document URI string. */
export interface DiagnosticData {
	readonly kind: "outdated";
	readonly targetRef: string;
	readonly range: vscode.Range;
	readonly newText: string;
}

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

/** Compute diagnostics for a document and publish them. */
export async function computeDiagnostics(
	uri: vscode.Uri,
	text: string,
	provider: ActupProvider,
	collection: vscode.DiagnosticCollection,
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
		const results = await checkRefs(text, provider);
		checked = { results, edits: planUpdates(results) };
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
		const diag = statusToDiagnostic(result.status, result.location, edit);
		if (diag === undefined) {
			continue;
		}

		diagnostics.push(diag.diagnostic);
		if (diag.data !== undefined) {
			dataMap.set(diagnosticKey(diag.diagnostic), diag.data);
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

interface DiagResult {
	diagnostic: vscode.Diagnostic;
	data?: DiagnosticData;
}

function statusToDiagnostic(
	status: CheckStatus,
	loc: UsesLocation,
	edit: UpdateEdit | undefined,
): DiagResult | undefined {
	// UpToDate — no diagnostic
	if (status === "UpToDate") return undefined;

	const line = loc.line - 1; // 1-indexed -> 0-indexed
	const refRange = loc.ref_range;
	const range = refRange !== null
		? new vscode.Range(line, byteToColumn(loc.raw_line, refRange.start), line, byteToColumn(loc.raw_line, refRange.end))
		: new vscode.Range(line, 0, line, loc.raw_line.length);

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
		const editRange = new vscode.Range(
			line,
			byteToColumn(loc.raw_line, edit.refStart),
			line,
			byteToColumn(loc.raw_line, edit.replaceEnd),
		);
		return { diagnostic: diag, data: { kind: "outdated", targetRef, range: editRange, newText: edit.newText } };
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
