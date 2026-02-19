import * as vscode from "vscode";
import { Executable, LanguageClient, LanguageClientOptions, ServerOptions, TransportKind } from "vscode-languageclient/node";

let client: LanguageClient | undefined;
let outputChannel: vscode.OutputChannel;

export async function activate(
	context: vscode.ExtensionContext,
): Promise<void> {
	outputChannel = vscode.window.createOutputChannel("actup");

	context.subscriptions.push(
		vscode.commands.registerCommand("actup.fixAllInWorkspace", fixAll),
	);
	context.subscriptions.push(
		vscode.commands.registerCommand("actup.restartServer", restartServer),
	);
	context.subscriptions.push(
		vscode.workspace.onDidChangeConfiguration((e) => {
			if (e.affectsConfiguration("actup.server")) {
				void restartServer();
			}
		}),
	);

	await startClient();
}

export async function deactivate(): Promise<void> {
	await stopClient();
	outputChannel?.dispose();
}

function serverCommand(): string {
	const config = vscode.workspace.getConfiguration("actup");
	return config.get<string | null>("server.path", null) ?? "actup-lsp";
}

function serverArgs(): string[] {
	const config = vscode.workspace.getConfiguration("actup");
	return config.get<string[]>("server.extraArgs", []);
}

async function startClient(): Promise<void> {
	const command = serverCommand();
	const args = serverArgs();

	const run: Executable = {
		command,
		args,
		transport: TransportKind.stdio,
	};
	const debug: Executable = {
		command,
		args,
		transport: TransportKind.stdio,
		options: {
			env: { ...process.env, RUST_LOG: "actup_lsp=debug" },
		},
	};
	const serverOptions: ServerOptions = { run, debug };

	const clientOptions: LanguageClientOptions = {
		documentSelector: [
			{ scheme: "file", language: "yaml", pattern: "**/.github/**" },
			{ scheme: "file", language: "yaml", pattern: "**/action.yml" },
			{ scheme: "file", language: "yaml", pattern: "**/action.yaml" },
		],
		outputChannel,
	};

	client = new LanguageClient(
		"actup-lsp",
		"actup",
		serverOptions,
		clientOptions,
	);
	await client.start();
	outputChannel.appendLine(`actup-lsp started (${command})`);
}

async function stopClient(): Promise<void> {
	if (client) {
		await client.stop();
		client = undefined;
	}
}

async function restartServer(): Promise<void> {
	outputChannel.appendLine("Restarting actup-lsp...");
	await stopClient();
	await startClient();
}

async function fixAll(): Promise<void> {
	if (!client?.isRunning()) {
		vscode.window.showWarningMessage("actup language server is not running.");
		return;
	}
	await client.sendRequest("workspace/executeCommand", {
		command: "actup.fixAllInWorkspace",
		arguments: [],
	});
}
