/**
 * Hand-maintained TypeScript types for actup-wasm.
 *
 * Only the provider contract ({@link ActupProvider} and
 * {@link ProviderErrorKind}) lives here — every other shape (input contracts
 * AND output wire types) is tsify-generated from Rust. See `index.ts` for
 * re-exports.
 */

import type { ActionYml, VersionSet } from "../wasm/actup_wasm.js";

/**
 * Why an {@link ActupProvider} call failed.
 *
 * `checkRefs` reads the `kind` property of a rejection: `notFound` marks the
 * repository or ref missing, `rateLimited`, `auth` and `forbidden` mark the
 * check blocked, and `network` or a missing `kind` marks it failed. A
 * `rateLimited` rejection may carry `retryAfter` in seconds.
 */
export type ProviderErrorKind = "notFound" | "rateLimited" | "auth" | "forbidden" | "network";

/**
 * Provider interface — implement to supply version data from any host.
 *
 * Reject with an object whose `kind` is a {@link ProviderErrorKind}.
 */
export interface ActupProvider {
	getVersions(host: string, owner: string, repo: string): Promise<VersionSet>;
	resolveRefToSha(host: string, owner: string, repo: string, ref: string): Promise<string>;
	getDefaultBranch(host: string, owner: string, repo: string): Promise<string>;
	getReadme(host: string, owner: string, repo: string, ref?: string | null): Promise<string | null>;
	getActionYml(host: string, owner: string, repo: string, ref: string): Promise<ActionYml>;
}
