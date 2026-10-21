import type { ActupProvider } from 'actup-wasm';
import { createGitHubProvider } from 'actup-wasm/provider';
import * as vscode from 'vscode';

/**
 * Create a GitHub provider using VS Code's authentication API for the token.
 *
 * Falls back to unauthenticated mode (60 req/hr) if no session is available.
 */
export async function createVSCodeGitHubProvider(): Promise<ActupProvider> {
	let token: string | undefined;
	try {
		const session = await vscode.authentication.getSession('github', ['repo'], {
			createIfNone: false,
			silent: true,
		});
		token = session?.accessToken;
	} catch (err) {
		console.error(`actup: GitHub auth lookup failed: ${err instanceof Error ? err.message : String(err)}`);
	}

	return createGitHubProvider({ token });
}
