/**
 * Hand-maintained TypeScript types for actup-wasm.
 *
 * Only the {@link ActupProvider} behavioral interface lives here — every
 * other shape (input contracts AND output wire types) is tsify-generated
 * from Rust. See `index.ts` for re-exports.
 */

import type { ActionYml, VersionSet } from "../wasm/actup_wasm.js";

/** Provider interface — implement to supply version data from any host. */
export interface ActupProvider {
	getVersions(host: string, owner: string, repo: string): Promise<VersionSet>;
	resolveRefToSha(host: string, owner: string, repo: string, ref: string): Promise<string>;
	getDefaultBranch(host: string, owner: string, repo: string): Promise<string>;
	getReadme(host: string, owner: string, repo: string, ref?: string | null): Promise<string | null>;
	getActionYml(host: string, owner: string, repo: string, ref: string): Promise<ActionYml>;
}
