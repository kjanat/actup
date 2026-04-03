import * as vscode from "vscode";
import { type DiagnosticData, diagnosticDataByUri, diagnosticKey } from "./diagnostics.js";

const SOURCE_FIX_ALL_ACTUP = vscode.CodeActionKind.SourceFixAll.append("actup");

const ACTION_SELECTOR: vscode.DocumentSelector = [
	{ scheme: "*", language: "github-actions-workflow" },
	{ scheme: "*", language: "yaml", pattern: "**/.*/workflows/*.yml" },
	{ scheme: "*", language: "yaml", pattern: "**/.*/workflows/*.yaml" },
	{ scheme: "*", language: "yaml", pattern: "**/action.yml" },
	{ scheme: "*", language: "yaml", pattern: "**/action.yaml" },
];

export { ACTION_SELECTOR };

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
		if (dataMap === undefined) return actions;

		const outdatedDiags: Array<{ diagnostic: vscode.Diagnostic; data: DiagnosticData }> = [];

		for (const diag of context.diagnostics) {
			if (diag.source !== "actup") continue;
			const data = dataMap.get(diagnosticKey(diag));
			if (data === undefined || data.kind !== "outdated") continue;
			outdatedDiags.push({ diagnostic: diag, data });
		}

		// Individual quickfixes
		for (const { diagnostic, data } of outdatedDiags) {
			const action = new vscode.CodeAction(
				`Update to ${data.targetRef}`,
				vscode.CodeActionKind.QuickFix,
			);
			const edit = new vscode.WorkspaceEdit();
			edit.replace(document.uri, diagnostic.range, data.targetRef);
			action.edit = edit;
			action.diagnostics = [diagnostic];
			action.isPreferred = true;
			actions.push(action);
		}

		// Fix-all source action when 2+ outdated refs exist in the file
		if (outdatedDiags.length >= 2) {
			const fixAll = new vscode.CodeAction(
				`Fix all outdated actions (${outdatedDiags.length} refs)`,
				SOURCE_FIX_ALL_ACTUP,
			);
			const edit = new vscode.WorkspaceEdit();
			for (const { diagnostic, data } of outdatedDiags) {
				edit.replace(document.uri, diagnostic.range, data.targetRef);
			}
			fixAll.edit = edit;
			fixAll.diagnostics = outdatedDiags.map(d => d.diagnostic);
			actions.push(fixAll);
		}

		return actions;
	}
}
