import * as vscode from "vscode";
import { type DiagnosticData, diagnosticDataByUri, diagnosticKey } from "./diagnostics.ts";

const SOURCE_FIX_ALL_ACTUP = vscode.CodeActionKind.SourceFixAll.append("actup");

const ACTION_SELECTOR: vscode.DocumentSelector = [
	{ scheme: "*", language: "github-actions-workflow" },
	{ scheme: "*", language: "yaml", pattern: "**/.*/workflows/*.yml" },
	{ scheme: "*", language: "yaml", pattern: "**/.*/workflows/*.yaml" },
	{ scheme: "*", language: "yaml", pattern: "**/action.yml" },
	{ scheme: "*", language: "yaml", pattern: "**/action.yaml" },
];

export { ACTION_SELECTOR };

function quickFixTitle(data: DiagnosticData): string {
	if (data.kind === "outdated") {
		return `Update to ${data.targetRef}`;
	}
	const sha = data.sha.slice(0, 7);
	return data.newText === data.sha ? `Pin to ${sha}` : `Pin to ${sha} # ${data.refName}`;
}

export class ActupCodeActionProvider implements vscode.CodeActionProvider {
	static readonly metadata: vscode.CodeActionProviderMetadata = {
		providedCodeActionKinds: [
			vscode.CodeActionKind.QuickFix,
			SOURCE_FIX_ALL_ACTUP,
		],
	};

	provideCodeActions(
		document: vscode.TextDocument,
		_range: vscode.Range | vscode.Selection,
		context: vscode.CodeActionContext,
	): vscode.CodeAction[] {
		const actions: vscode.CodeAction[] = [];
		const dataMap = diagnosticDataByUri.get(document.uri.toString());
		if (dataMap === undefined) {
			return actions;
		}

		const fixable: { diagnostic: vscode.Diagnostic; data: DiagnosticData }[] = [];

		for (const diag of context.diagnostics) {
			if (diag.source !== "actup") continue;
			const data = dataMap.get(diagnosticKey(diag));
			if (data === undefined) continue;
			fixable.push({ diagnostic: diag, data });
		}

		// Individual quickfixes
		for (const { diagnostic, data } of fixable) {
			const action = new vscode.CodeAction(quickFixTitle(data), vscode.CodeActionKind.QuickFix);
			const edit = new vscode.WorkspaceEdit();
			edit.replace(document.uri, data.range, data.newText);
			action.edit = edit;
			action.diagnostics = [diagnostic];
			action.isPreferred = true;
			actions.push(action);
		}

		// Fix-all uses every fixable diagnostic in the document, not just those
		// in `context.diagnostics` (which is range-scoped to the cursor).
		const allFixable: { diagnostic: vscode.Diagnostic; data: DiagnosticData }[] = [];
		for (const diag of vscode.languages.getDiagnostics(document.uri)) {
			if (diag.source !== "actup") continue;
			const data = dataMap.get(diagnosticKey(diag));
			if (data === undefined) continue;
			allFixable.push({ diagnostic: diag, data });
		}

		if (allFixable.length >= 2) {
			const fixAll = new vscode.CodeAction(
				`Fix all outdated actions (${allFixable.length} refs)`,
				SOURCE_FIX_ALL_ACTUP,
			);
			const edit = new vscode.WorkspaceEdit();
			for (const { data } of allFixable) {
				edit.replace(document.uri, data.range, data.newText);
			}
			fixAll.edit = edit;
			fixAll.diagnostics = allFixable.map(d => d.diagnostic);
			actions.push(fixAll);
		}

		return actions;
	}
}
