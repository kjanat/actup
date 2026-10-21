import type { ActupProvider } from 'actup-wasm';
import * as vscode from 'vscode';
import { ACTION_SELECTOR, ActupCodeActionProvider } from './codeActions.ts';
import { ActupCompletionProvider } from './completion.ts';
import { type ConfigLookup, ConfigStore, watchConfig } from './config.ts';
import { checkResultsByUri, clearDiagnosticData, computeDiagnostics, createDiagnosticCollection } from './diagnostics.ts';
import { DocumentTracker } from './documentTracker.ts';
import { ActupHoverProvider } from './hover.ts';
import { byteToColumn } from './position.ts';
import { createVSCodeGitHubProvider } from './provider.ts';
import { ensureWasmInitialized } from './wasmLoader.ts';

let outputChannel: vscode.LogOutputChannel;

export async function activate(
	context: vscode.ExtensionContext,
): Promise<void> {
	outputChannel = vscode.window.createOutputChannel('actup', { log: true });

	// Initialize WASM
	try {
		await ensureWasmInitialized(context);
		outputChannel.info('actup WASM engine initialized');
	} catch (err) {
		const message = err instanceof Error ? err.message : String(err);
		outputChannel.error(`Failed to initialize actup WASM: ${message}`);
		vscode.window.showErrorMessage(
			'actup: Failed to initialize WASM engine. See output for details.',
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
		ghProvider = (await import('actup-wasm/provider')).createGitHubProvider();
	}

	// Diagnostics
	const diagnosticCollection = createDiagnosticCollection();
	const configs = new ConfigStore((path, message) => {
		outputChannel.error(`actup: invalid config ${path}: ${message}`);
		void vscode.window.showErrorMessage(`actup: invalid config ${path}. See output for details.`);
	});

	// Register language providers
	context.subscriptions.push(
		vscode.languages.registerCodeActionsProvider(
			ACTION_SELECTOR,
			new ActupCodeActionProvider(),
			ActupCodeActionProvider.metadata,
		),
		vscode.languages.registerHoverProvider(
			ACTION_SELECTOR,
			new ActupHoverProvider(ghProvider, checkResultsByUri),
		),
		vscode.languages.registerCompletionItemProvider(
			ACTION_SELECTOR,
			new ActupCompletionProvider(ghProvider, outputChannel),
			'@',
		),
		diagnosticCollection,
		outputChannel,
	);

	// Document tracking
	const tracker = new DocumentTracker((uri, text) => {
		void computeDiagnostics(uri, text, ghProvider, diagnosticCollection, configs.configFor);
	});
	const reanalyze = (): void => {
		for (const doc of vscode.workspace.textDocuments) {
			if (DocumentTracker.isActionDocument(doc)) {
				tracker.open(doc);
			}
		}
	};
	context.subscriptions.push(...watchConfig(configs, reanalyze));

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
		}),
	);

	// Analyze already-open documents
	reanalyze();

	// Commands
	context.subscriptions.push(
		vscode.commands.registerCommand(
			'actup.fixAllInWorkspace',
			() => fixAllInWorkspace(ghProvider, outputChannel, configs.configFor),
		),
	);

	outputChannel.info('actup web extension activated');
}

export function deactivate(): void {}

/**
 * Apply every planned update to the workspace's workflow and action files.
 *
 * A file under an invalid repo config counts as failed and keeps its text.
 */
export async function fixAllInWorkspace(
	provider: ActupProvider,
	log: vscode.LogOutputChannel,
	configs: ConfigLookup,
): Promise<void> {
	const [workflowFiles, compositeFiles] = await Promise.all([
		vscode.workspace.findFiles('**/.github/workflows/*.{yml,yaml}', '**/node_modules/**'),
		vscode.workspace.findFiles('**/.github/actions/**/action.{yml,yaml}', '**/node_modules/**'),
	]);
	const files = [...workflowFiles, ...compositeFiles];
	if (files.length === 0) {
		vscode.window.showInformationMessage('actup: No workflow files found.');
		return;
	}

	let totalEdits = 0;
	const failedFiles: { uri: vscode.Uri; message: string }[] = [];
	for (const fileUri of files) {
		try {
			const doc = await vscode.workspace.openTextDocument(fileUri);
			const text = doc.getText();
			if (!text.includes('uses:')) continue;

			const folderConfig = await configs(fileUri);
			if (folderConfig.kind === 'invalid') {
				throw new Error(`invalid config ${folderConfig.path}: ${folderConfig.message}`);
			}
			const config = folderConfig.kind === 'loaded' ? folderConfig.config : undefined;
			const { checkRefs, planUpdates } = await import('actup-wasm');
			const results = await checkRefs(text, provider, config);
			const edits = planUpdates(results, config);
			if (edits.length === 0) continue;

			const wsEdit = new vscode.WorkspaceEdit();
			for (const edit of edits) {
				const line = edit.line - 1;
				const lineText = doc.lineAt(line).text;
				const range = new vscode.Range(
					line,
					byteToColumn(lineText, edit.refStart),
					line,
					byteToColumn(lineText, edit.replaceEnd),
				);
				wsEdit.replace(fileUri, range, edit.newText);
			}
			await vscode.workspace.applyEdit(wsEdit);
			totalEdits += edits.length;
		} catch (err) {
			const message = err instanceof Error ? err.message : String(err);
			failedFiles.push({ uri: fileUri, message });
			log.error(`actup: failed to process ${fileUri.toString()}: ${message}`);
		}
	}

	const failureSummary = failedFiles.length > 0
		? ` (${failedFiles.length} file(s) failed; see actup output)`
		: '';
	if (totalEdits > 0) {
		vscode.window.showInformationMessage(
			`actup: Updated ${totalEdits} action ref(s) across ${files.length} file(s)${failureSummary}.`,
		);
	} else if (failedFiles.length > 0) {
		vscode.window.showWarningMessage(
			`actup: No updates applied${failureSummary}.`,
		);
	} else {
		vscode.window.showInformationMessage('actup: All actions are up to date.');
	}
}
