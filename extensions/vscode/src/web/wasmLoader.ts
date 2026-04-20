import { initWasm } from "actup-wasm";
import * as vscode from "vscode";

let initialized = false;

/** Initialize the WASM module from the extension's bundled binary. */
export async function ensureWasmInitialized(
	context: vscode.ExtensionContext,
): Promise<void> {
	if (initialized) {
		return;
	}

	const wasmUri = vscode.Uri.joinPath(
		context.extensionUri,
		"dist",
		"web",
		"wasm",
		"actup_wasm_bg.wasm",
	);

	const wasmBytes = await vscode.workspace.fs.readFile(wasmUri);
	await initWasm(wasmBytes.buffer as ArrayBuffer);
	initialized = true;
}
