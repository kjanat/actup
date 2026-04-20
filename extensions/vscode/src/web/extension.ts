import type { ActupProvider, CheckResult } from "actup-wasm";
import * as vscode from "vscode";
import { ACTION_SELECTOR, ActupCodeActionProvider } from "./codeActions.ts";
import { ActupCompletionProvider } from "./completion.ts";
import { clearDiagnosticData, computeDiagnostics, createDiagnosticCollection } from "./diagnostics.ts";
import { DocumentTracker } from "./documentTracker.ts";
import { ActupHoverProvider } from "./hover.ts";
import { createVSCodeGitHubProvider } from "./provider.ts";
import { ensureWasmInitialized } from "./wasmLoader.ts";

let outputChannel: vscode.LogOutputChannel;

export async function activate(
	context: vscode.ExtensionContext,
): Promise<void> {
	outputChannel = vscode.window.createOutputChannel("actup", { log: true });

	// Initialize WASM
	try {
		await ensureWasmInitialized(context);
		outputChannel.info("actup WASM engine initialized");
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		outputChannel.error(`Failed to initialize actup WASM: ${message}`);
		vscode.window.showErrorMessage(
			"actup: Failed to initialize WASM engine. See output for details.",
		);
		return;
	}

	// Create GitHub provider with VS Code auth
	let ghProvider: ActupProvider;
	try {
		ghProvider = await createVSCodeGitHubProvider();
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		outputChannel.error(`Failed to create GitHub provider: ${message}`);
		ghProvider = (await import("actup-wasm/provider")).createGitHubProvider();
	}

	// Result cache for hover and code actions
	const resultCache = new Map<string, CheckResult[]>();

	// Diagnostics
	const diagnosticCollection = createDiagnosticCollection();

	// Register language providers
	context.subscriptions.push(
		vscode.languages.registerCodeActionsProvider(
			ACTION_SELECTOR,
			new ActupCodeActionProvider(),
			ActupCodeActionProvider.metadata,
		),
		vscode.languages.registerHoverProvider(
			ACTION_SELECTOR,
			new ActupHoverProvider(ghProvider, resultCache),
		),
		vscode.languages.registerCompletionItemProvider(
			ACTION_SELECTOR,
			new ActupCompletionProvider(ghProvider),
			"@",
		),
		diagnosticCollection,
		outputChannel,
	);

	// Document tracking
	const tracker = new DocumentTracker((uri, text) => {
		void computeDiagnostics(uri, text, ghProvider, diagnosticCollection);
	});

	context.subscriptions.push(
		vscode.workspace.onDidOpenTextDocument((doc) => {
			if (DocumentTracker.isActionDocument(doc)) {
				tracker.open(doc);
			}
		}),
		vscode.workspace.onDidChangeTextDocument((e) => {
			if (DocumentTracker.isActionDocument(e.document)) {
				tracker.change(e.document);
			}
		}),
		vscode.workspace.onDidCloseTextDocument((doc) => {
			tracker.close(doc.uri);
			diagnosticCollection.delete(doc.uri);
			clearDiagnosticData(doc.uri);
			resultCache.delete(doc.uri.toString());
		}),
	);

	// Analyze already-open documents
	for (const doc of vscode.workspace.textDocuments) {
		if (DocumentTracker.isActionDocument(doc)) {
			tracker.open(doc);
		}
	}

	// Commands
	context.subscriptions.push(
		vscode.commands.registerCommand("actup.fixAllInWorkspace", () => fixAllInWorkspace(ghProvider)),
	);

	outputChannel.info("actup web extension activated");
}

export function deactivate(): void {}

async function fixAllInWorkspace(
	provider: ActupProvider,
): Promise<void> {
	const files = await vscode.workspace.findFiles(
		"**/.github/workflows/*.{yml,yaml}",
		"**/node_modules/**",
	);
	if (files.length === 0) {
		vscode.window.showInformationMessage("actup: No workflow files found.");
		return;
	}

	let totalEdits = 0;
	for (const fileUri of files) {
		const doc = await vscode.workspace.openTextDocument(fileUri);
		const text = doc.getText();
		if (!text.includes("uses:")) continue;

		const { checkRefs, planUpdates } = await import("actup-wasm");
		const results = await checkRefs(text, provider);
		const edits = planUpdates(text, results);
		if (edits.length === 0) continue;

		const wsEdit = new vscode.WorkspaceEdit();
		for (const edit of edits) {
			const line = edit.line - 1;
			const range = new vscode.Range(line, edit.refStart, line, edit.refEnd);
			wsEdit.replace(fileUri, range, edit.newRef);
		}
		await vscode.workspace.applyEdit(wsEdit);
		totalEdits += edits.length;
	}

	if (totalEdits > 0) {
		vscode.window.showInformationMessage(
			`actup: Updated ${totalEdits} action ref(s) across ${files.length} file(s).`,
		);
	} else {
		vscode.window.showInformationMessage("actup: All actions are up to date.");
	}
}
