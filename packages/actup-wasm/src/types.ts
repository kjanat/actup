/**
 * Hand-maintained TypeScript types for actup-wasm.
 *
 * These types describe the **input contracts** that JS code must satisfy:
 * the {@link ActupProvider} interface that callers implement, and the
 * shapes the provider returns ({@link VersionSet}, {@link ActionYml}) and
 * the JSON config callers pass in ({@link ActupConfig}).
 *
 * Output types (parsed from Rust→serde→JS) live in the wasm-bindgen
 * generated `.d.ts` and are emitted by the `tsify-next` derive macros on
 * the Rust types — see `index.ts` for the re-exports.
 */

/** Provider interface — implement to supply version data from any host. */
export interface ActupProvider {
	getVersions(host: string, owner: string, repo: string): Promise<VersionSet>;
	resolveRefToSha(host: string, owner: string, repo: string, ref: string): Promise<string>;
	getDefaultBranch(host: string, owner: string, repo: string): Promise<string>;
	getReadme(host: string, owner: string, repo: string, ref?: string | null): Promise<string | null>;
	getActionYml(host: string, owner: string, repo: string, ref: string): Promise<ActionYml>;
}

/** Version set returned by provider's getVersions. */
export interface VersionSet {
	readonly semverTags: ReadonlyArray<{ readonly version: string; readonly sha: string }>;
	readonly defaultBranch?: { readonly name: string; readonly sha: string } | undefined;
}

/** Parsed action.yml metadata. */
export interface ActionYml {
	readonly raw: string;
	readonly name: string | null;
	readonly description: string | null;
	readonly author: string | null;
	readonly inputs: Record<string, ActionInput>;
	readonly outputs: Record<string, ActionOutput>;
	readonly runs: ActionRuns | null;
}

/** Action input definition. */
export interface ActionInput {
	readonly description: string;
	readonly required: boolean;
	readonly default?: string | undefined;
	readonly deprecation_message?: string | undefined;
}

/** Action output definition. */
export interface ActionOutput {
	readonly description: string;
	readonly value?: string | undefined;
}

/** Action runs configuration. */
export interface ActionRuns {
	readonly using: string;
	readonly main?: string | undefined;
	readonly pre?: string | undefined;
	readonly post?: string | undefined;
	readonly image?: string | undefined;
	readonly steps?: ReadonlyArray<ActionStep> | undefined;
}

/** A step in a composite action. */
export interface ActionStep {
	readonly uses?: string | undefined;
	readonly run?: string | undefined;
	readonly name?: string | undefined;
}

/** Subset of actup config passable to WASM functions. */
export interface ActupConfig {
	readonly default_host?: string | undefined;
	readonly update_policy?: "patch" | "minor" | "major" | "latest" | undefined;
}
