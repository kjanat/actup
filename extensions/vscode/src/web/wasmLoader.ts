import { initWasm } from "actup-wasm";
import * as vscode from "vscode";

let initPromise: Promise<void> | null = null;

/** Initialize the WASM module from the extension's bundled binary. */
export function ensureWasmInitialized(
	context: vscode.ExtensionContext,
): Promise<void> {
	if (initPromise !== null) {
		return initPromise;
	}

	initPromise = (async () => {
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
	})();
	return initPromise;
}
