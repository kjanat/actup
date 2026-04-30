/**
 * actup-wasm — Parse and check GitHub Actions `uses:` refs.
 *
 * Works in Node.js 18+, Bun, Deno, and browsers.
 *
 * @example
 * ```ts
 * import { initWasm, parseUsesRefs } from "actup-wasm";
 *
 * await initWasm();
 * const locations = parseUsesRefs("uses: actions/checkout@v4");
 * ```
 *
 * @packageDocumentation
 */

/// <reference types="node" />

import type { ActupConfig, ActupProvider, CheckResult, UpdateEdit, UsesLocation } from "./types.ts";

export type { GitHubProviderOptions } from "./provider.ts";

export { createGitHubProvider, GitHubProviderError } from "./provider.ts";
export type {
	ActionInput,
	ActionOutput,
	ActionRef,
	ActionRuns,
	ActionStep,
	ActionYml,
	ActupConfig,
	ActupProvider,
	CheckResult,
	CheckStatus,
	OutdatedInfo,
	UnresolvableInfo,
	UpdateEdit,
	UsesLocation,
	VersionInfo,
	VersionSet,
} from "./types.ts";

/**
 * Low-level wasm-bindgen bindings.
 *
 * The `--target web` build exports an `init()` default export for async
 * initialization and `initSync()` for synchronous initialization with a
 * buffer. The actual WASM functions (`parseUsesRefs`, `checkRefs`,
 * `planUpdates`) are only usable after initialization.
 */

type WasmBindings = typeof import("../wasm/actup_wasm.js");

let wasm: WasmBindings | undefined;
let initPromise: Promise<void> | undefined;

/**
 * Initialize the WASM module.
 *
 * - **Node.js / Bun / Deno**: reads the `.wasm` file from disk via `fs.readFileSync`
 *   and initializes synchronously via `initSync`.
 * - **Browser**: fetches the `.wasm` URL and initializes asynchronously via `init`.
 *
 * Safe to call multiple times — subsequent calls are no-ops.
 *
 * @param wasmSource - Optional: a URL, fetch Response, or ArrayBuffer of the
 *   `.wasm` file. If omitted, auto-detected from the environment.
 */
export async function initWasm(wasmSource?: URL | Response | BufferSource): Promise<void> {
	if (wasm !== undefined) return;
	initPromise = initPromise ?? doInit(wasmSource);
	await initPromise;
}

async function doInit(wasmSource?: URL | Response | BufferSource): Promise<void> {
	// Dynamic import so the wasm glue JS isn't evaluated at module load time.
	// The path is relative to the compiled output in dist/.
	const mod = await import("../wasm/actup_wasm.js");

	if (wasmSource !== undefined) {
		if (wasmSource instanceof ArrayBuffer || ArrayBuffer.isView(wasmSource)) {
			mod.initSync({ module: wasmSource });
		} else {
			await mod.default(wasmSource);
		}
	} else if (isNodeLike()) {
		// Node.js / Bun / Deno with node compat — read .wasm from disk
		const wasmBytes = await loadWasmNode();
		mod.initSync({ module: wasmBytes });
	} else {
		// Browser — use default init which fetches via import.meta.url
		await mod.default();
	}

	wasm = mod;
}

/** Ensure WASM is initialized, throwing if not. */
function requireInit(): WasmBindings {
	if (wasm === undefined) {
		throw new Error(
			"actup-wasm not initialized. Call `await initWasm()` before using any functions.",
		);
	}
	return wasm;
}

/**
 * Parse `uses:` references from workflow YAML content.
 *
 * Pure, synchronous. No network or filesystem access.
 *
 * @param yaml - Raw YAML content of a workflow or composite action file.
 * @returns Array of `uses:` reference locations found in the YAML.
 */
export function parseUsesRefs(yaml: string): UsesLocation[] {
	const w = requireInit();
	return w.parseUsesRefs(yaml);
}

/**
 * Check `uses:` references in YAML against a provider.
 *
 * Parses the YAML, resolves each non-local `uses:` ref through the provider,
 * and returns an array of check results with status (up-to-date, outdated,
 * unresolvable).
 *
 * @param yaml - Raw YAML content.
 * @param provider - Object implementing the `ActupProvider` interface.
 * @param config - Optional config overrides (update policy, default host).
 * @returns Array of check results.
 */
export async function checkRefs(
	yaml: string,
	provider: ActupProvider,
	config?: ActupConfig,
): Promise<CheckResult[]> {
	const w = requireInit();
	const configJson = config !== undefined ? JSON.stringify(config) : undefined;
	return await w.checkRefs(yaml, provider, configJson);
}

/**
 * Plan minimal text edits from check results.
 *
 * Takes the original YAML and a JSON array of check results (as returned by
 * `checkRefs`), and returns an array of edit objects for each outdated ref.
 *
 * Pure, synchronous. The caller applies the edits.
 *
 * @param yaml - The original YAML content.
 * @param results - Array of check results from `checkRefs`.
 * @returns Array of text edits to apply.
 */
export function planUpdates(yaml: string, results: CheckResult[]): UpdateEdit[] {
	const w = requireInit();
	const resultsJson = JSON.stringify(results);
	return w.planUpdates(yaml, resultsJson);
}

/** Detect Node.js / Bun / Deno (with node compat) runtime. */
function isNodeLike(): boolean {
	return (
		typeof globalThis.process !== "undefined"
		&& typeof globalThis.process.versions !== "undefined"
		&& typeof globalThis.process.versions.node === "string"
	);
}

/** Load .wasm bytes from disk in Node.js-like runtimes. */
async function loadWasmNode(): Promise<Uint8Array> {
	const { readFileSync } = await import("node:fs");
	const { fileURLToPath } = await import("node:url");

	const wasmPath = fileURLToPath(new URL("../wasm/actup_wasm_bg.wasm", import.meta.url));
	return new Uint8Array(readFileSync(wasmPath));
}
