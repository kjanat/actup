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
	// .slice() narrows Uint8Array<ArrayBufferLike> to Uint8Array<ArrayBuffer>,
	// which is what BufferSource wants in TS 5.7+. Copies ~290KB once at activation.
	await initWasm(wasmBytes.slice());
	initialized = true;
}
