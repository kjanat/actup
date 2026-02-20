/**
 * TypeScript type definitions for actup-wasm.
 *
 * These mirror the Rust types serialized via serde-wasm-bindgen.
 * The wasm-bindgen generated `.d.ts` types everything as `any`;
 * this module provides the actual shapes.
 */

/** A `uses:` reference location extracted from YAML content. */
export interface UsesLocation {
	/** 1-indexed line number in the YAML file. */
	readonly line: number;
	/** 1-indexed column number where the value starts. */
	readonly column: number;
	/** The `uses:` value, e.g. `"actions/checkout@v4"`. */
	readonly value: string;
	/** The full raw line from the YAML file. */
	readonly raw_line: string;
	/** Byte range of the ref portion (after `@`) within the value. */
	readonly ref_range: { readonly start: number; readonly end: number } | null;
}

/** Parsed action reference (owner/repo@ref with optional host/subpath). */
export interface ActionRef {
	readonly host: string | null;
	readonly owner: string;
	readonly repo: string;
	readonly subpath: string | null;
	readonly git_ref: string;
}

/** Result of checking a single `uses:` reference. */
export interface CheckResult {
	readonly action_ref: ActionRef;
	readonly status: CheckStatus;
}

/** Discriminated union of check outcomes. Serialized as serde externally-tagged enum. */
export type CheckStatus =
	| "UpToDate"
	| { readonly Outdated: OutdatedInfo }
	| { readonly Unresolvable: UnresolvableInfo };

/** Details for an outdated ref. */
export interface OutdatedInfo {
	readonly target: VersionInfo;
	readonly current_version: string | null;
}

/** Version info from the target. */
export interface VersionInfo {
	readonly version: string;
	readonly sha: string | null;
}

/** Details for an unresolvable ref. */
export interface UnresolvableInfo {
	readonly reason: string;
	readonly is_environment_error: boolean;
}

/** A text edit to apply to YAML content. */
export interface UpdateEdit {
	/** 1-indexed line number. */
	readonly line: number;
	/** 0-indexed byte offset of the ref start within the line. */
	readonly refStart: number;
	/** 0-indexed byte offset of the ref end within the line. */
	readonly refEnd: number;
	/** The full original `uses:` value. */
	readonly original: string;
	/** The replacement ref string. */
	readonly newRef: string;
}

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
