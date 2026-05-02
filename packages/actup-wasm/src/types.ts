/**
 * Hand-maintained TypeScript types for actup-wasm.
 *
 * These types describe **input contracts** that JS code must satisfy:
 * the {@link ActupProvider} behavioral interface (callers implement it),
 * the {@link VersionSet} shape its `getVersions` returns, and the JSON
 * config callers pass in ({@link ActupConfig}).
 *
 * The data shapes that travel out of Rust (and the action.yml structure
 * the provider returns from `getActionYml`) are tsify-generated from the
 * Rust types — see `index.ts` for the re-exports.
 */

import type { ActionYml } from "../wasm/actup_wasm.js";

/** Provider interface — implement to supply version data from any host. */
export interface ActupProvider {
	getVersions(host: string, owner: string, repo: string): Promise<VersionSet>;
	resolveRefToSha(host: string, owner: string, repo: string, ref: string): Promise<string>;
	getDefaultBranch(host: string, owner: string, repo: string): Promise<string>;
	getReadme(host: string, owner: string, repo: string, ref?: string | null): Promise<string | null>;
	getActionYml(host: string, owner: string, repo: string, ref: string): Promise<ActionYml>;
}

/**
 * Version set returned by a provider's `getVersions`.
 *
 * Shape mirrors the Rust-side `JsVersionSet` deserializer (camelCase). Kept
 * hand-typed because tsify has no container-level rename, and we don't want
 * to expose `Js*` names on the public TS surface.
 */
export interface VersionSet {
	readonly semverTags: ReadonlyArray<{ readonly version: string; readonly sha: string }>;
	readonly defaultBranch?: { readonly name: string; readonly sha: string } | undefined;
}

/** Subset of actup config passable to WASM functions. */
export interface ActupConfig {
	readonly default_host?: string | undefined;
	readonly update_policy?: "patch" | "minor" | "major" | "latest" | undefined;
}
