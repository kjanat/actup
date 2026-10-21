import * as vscode from 'vscode';
import {
	type Executable,
	LanguageClient,
	type LanguageClientOptions,
	type ServerOptions,
	TransportKind,
} from 'vscode-languageclient/node.js';
import { configPathSetting, initializationOptions, restartsServer } from './settings.ts';

let client: LanguageClient | undefined;
let outputChannel: vscode.LogOutputChannel;

export async function activate(
	context: vscode.ExtensionContext,
): Promise<void> {
	outputChannel = vscode.window.createOutputChannel('actup', { log: true });

	context.subscriptions.push(
		vscode.commands.registerCommand('actup.fixAllInWorkspace', fixAll),
	);
	context.subscriptions.push(
		vscode.commands.registerCommand('actup.restartServer', restartServer),
	);
	context.subscriptions.push(
		vscode.workspace.onDidChangeConfiguration((e) => {
			if (restartsServer(e)) {
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
	const config = vscode.workspace.getConfiguration('actup');
	return config.get<string | null>('server.path', null) ?? 'actup-lsp';
}

function serverArgs(): string[] {
	const config = vscode.workspace.getConfiguration('actup');
	return config.get<string[]>('server.extraArgs', []);
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
			env: { ...process.env, RUST_LOG: 'actup_lsp=debug' },
		},
	};
	const serverOptions: ServerOptions = { run, debug };

	const clientOptions: LanguageClientOptions = {
		documentSelector: [
			{ scheme: 'file', language: 'github-actions-workflow' },
			{ scheme: 'file', language: 'yaml', pattern: '**/.*/workflows/*.yml' },
			{ scheme: 'file', language: 'yaml', pattern: '**/.*/workflows/*.yaml' },
			{ scheme: 'file', language: 'yaml', pattern: '**/action.yml' },
			{ scheme: 'file', language: 'yaml', pattern: '**/action.yaml' },
		],
		initializationOptions: initializationOptions(configPathSetting()),
		outputChannel,
	};

	client = new LanguageClient(
		'actup-lsp',
		'actup',
		serverOptions,
		clientOptions,
	);
	try {
		await client.start();
		outputChannel.info(`actup-lsp started (${command})`);
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		outputChannel.error(`Failed to start actup-lsp: ${message}`);
		void vscode.window.showErrorMessage(
			"actup: Failed to start language server. Is 'actup-lsp' installed? See output for details.",
		);
		client = undefined;
	}
}

async function stopClient(): Promise<void> {
	if (client) {
		await client.stop();
		client = undefined;
	}
}

async function restartServer(): Promise<void> {
	outputChannel.info('Restarting actup-lsp...');
	await stopClient();
	await startClient();
}

async function fixAll(): Promise<void> {
	if (!client?.isRunning()) {
		vscode.window.showWarningMessage('actup language server is not running.');
		return;
	}
	try {
		await client.sendRequest('workspace/executeCommand', {
			command: 'actup-lsp/fixAllInWorkspace',
			arguments: [],
		});
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		outputChannel.error(`actup-lsp/fixAllInWorkspace failed: ${message}`);
		void vscode.window.showErrorMessage('actup: Fix all failed. See output for details.');
	}
}
