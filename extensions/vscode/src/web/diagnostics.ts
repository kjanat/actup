import type { ActupProvider, UsesLocation } from "actup-wasm";
import { checkRefs, type CheckStatus, type LocatedCheckResult } from "actup-wasm";
import * as vscode from "vscode";
import { byteToColumn } from "./position.ts";

const DIAGNOSTIC_SOURCE = "actup";

/** Stored data for outdated diagnostics, keyed by document URI string. */
export interface DiagnosticData {
	readonly kind: "outdated";
	readonly targetRef: string;
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

	let results: LocatedCheckResult[] | undefined;
	try {
		results = await checkRefs(text, provider);
	} catch {
		results = undefined;
	}

	if (latestComputeByUri.get(key) !== compute) {
		return;
	}
	if (results === undefined) {
		// Network/provider error — clear diagnostics rather than show stale data
		collection.set(uri, []);
		clearDiagnosticData(uri);
		return;
	}

	const diagnostics: vscode.Diagnostic[] = [];
	const dataMap = new Map<string, DiagnosticData>();

	for (const result of results) {
		const diag = statusToDiagnostic(result.status, result.location);
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
		return refRange === null ? { diagnostic: diag } : { diagnostic: diag, data: { kind: "outdated", targetRef } };
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
