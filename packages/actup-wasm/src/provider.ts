/**
 * Default fetch()-based GitHub provider.
 *
 * Implements the {@link ActupProvider} interface using the GitHub REST API.
 * Works in browsers, Node.js 18+, Bun, Deno, and Cloudflare Workers — any
 * runtime with a global `fetch`.
 *
 * @module
 */

import type { ActionYml, VersionSet } from "../wasm/actup_wasm.js";
import type { ActupProvider, ProviderErrorKind } from "./types.ts";

/** Options for {@link createGitHubProvider}. */
export interface GitHubProviderOptions {
	/**
	 * Personal access token or fine-grained token for authentication.
	 * Without a token, requests are subject to 60 req/hr IP-based rate limits.
	 */
	readonly token?: string | undefined;
	/**
	 * API base URL override. When set, this base is used for all requests
	 * regardless of the per-call `host` argument — useful for single-host
	 * setups (e.g., a single GitHub Enterprise instance).
	 *
	 * When omitted, the base is derived from each call's `host`:
	 * `github.com` → `https://api.github.com`; any other host →
	 * `https://{host}/api/v3` (the GHE convention). This enables
	 * mixed-host workflows that reference both `github.com` and a
	 * private GHE.
	 */
	readonly apiBase?: string | undefined;
	/**
	 * Maximum tags to fetch per page (1–100). Defaults to 100.
	 */
	readonly perPage?: number | undefined;
}

/** Structured error from the GitHub provider. */
export class GitHubProviderError extends Error {
	override readonly name = "GitHubProviderError";
	readonly kind: ProviderErrorKind;
	readonly status: number;
	readonly rateLimitRemaining: number | undefined;
	readonly retryAfter: number | undefined;

	constructor(
		message: string,
		kind: ProviderErrorKind,
		status: number,
		options?: {
			readonly rateLimitRemaining?: number | undefined;
			readonly retryAfter?: number | undefined;
		},
	) {
		super(message);
		this.kind = kind;
		this.status = status;
		this.rateLimitRemaining = options?.rateLimitRemaining;
		this.retryAfter = options?.retryAfter;
	}
}

/**
 * Create a GitHub REST API provider.
 *
 * @example
 * ```ts
 * import { createGitHubProvider } from "actup-wasm/provider";
 * import { initWasm, checkRefs } from "actup-wasm";
 *
 * await initWasm();
 * const provider = createGitHubProvider({ token: process.env.GITHUB_TOKEN });
 * const results = await checkRefs(yaml, provider);
 * ```
 */
export function createGitHubProvider(options?: GitHubProviderOptions): ActupProvider {
	const configuredApiBase = options?.apiBase !== undefined ? normalizeBase(options.apiBase) : undefined;
	const perPage = clamp(options?.perPage ?? 100, 1, 100);
	const headers = buildHeaders(options?.token);

	const resolveApiBase = (host: string): string => {
		// Explicit user config wins for all hosts (single-host mode).
		if (configuredApiBase !== undefined) return configuredApiBase;
		// Empty or `github.com` → public REST endpoint.
		if (host === "" || host === "github.com") return "https://api.github.com";
		// Anything else: GHE convention.
		return `https://${host}/api/v3`;
	};

	return {
		getVersions: (host, owner, repo) => getVersions(resolveApiBase(host), headers, perPage, owner, repo),
		resolveRefToSha: (host, owner, repo, ref) => resolveRefToSha(resolveApiBase(host), headers, owner, repo, ref),
		getDefaultBranch: (host, owner, repo) => getDefaultBranch(resolveApiBase(host), headers, owner, repo),
		getReadme: (host, owner, repo, ref) => getReadme(resolveApiBase(host), headers, owner, repo, ref ?? undefined),
		getActionYml: (host, owner, repo, ref) => getActionYml(resolveApiBase(host), headers, owner, repo, ref),
	};
}

// -- Implementation --

/** Build standard headers for GitHub REST API requests. */
function buildHeaders(
	/** Optional token for Authorization header. */
	token: string | undefined,
	/** Optional Accept header. @default "application/vnd.github+json". */
	accept: string = "application/vnd.github+json",
	/** Optional client version for User-Agent. @default "0.1.0". */
	version: string = "0.1.0",
	/** Optional API version for X-GitHub-Api-Version. @default "2022-11-28". */
	apiVersion: string = "2022-11-28",
): Record<string, string> {
	const h: Record<string, string> = {
		"Accept": accept,
		"X-GitHub-Api-Version": apiVersion,
		"User-Agent": `actup-wasm/${version}`,
	};
	if (token !== undefined && token.length > 0) {
		h.Authorization = `Bearer ${token}`;
	}
	return h;
}

/** Strip trailing slash from base URL. */
function normalizeBase(base: string): string {
	return base.endsWith("/") ? base.slice(0, -1) : base;
}

/** Clamp a number to [min, max]. */
function clamp(n: number, min: number, max: number): number {
	return Math.max(min, Math.min(max, Math.trunc(n)));
}

// -- GitHub REST response types (internal) --

interface GhTag {
	readonly name: string;
	readonly commit: { readonly sha: string };
}

interface GhRepo {
	readonly default_branch: string;
}

interface GhBranch {
	readonly name: string;
	readonly commit: { readonly sha: string };
}

interface GhRef {
	readonly ref: string;
	readonly object: { readonly type: string; readonly sha: string };
}

interface GhTagObject {
	readonly object: { readonly type: string; readonly sha: string };
}

interface GhContent {
	readonly type: string;
	readonly content?: string | undefined;
	readonly encoding?: string | undefined;
}

// -- Runtime type guards (validate untyped fetch responses) --

function isObject(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function hasString(obj: Record<string, unknown>, key: string): boolean {
	return typeof obj[key] === "string";
}

function isCommitSha(obj: unknown): obj is { readonly sha: string } {
	return isObject(obj) && hasString(obj, "sha");
}

function isGhTag(value: unknown): value is GhTag {
	return isObject(value) && hasString(value, "name") && isCommitSha(value.commit);
}

function isGhTagArray(value: unknown): value is readonly GhTag[] {
	return Array.isArray(value) && value.every(isGhTag);
}

function isGhRepo(value: unknown): value is GhRepo {
	return isObject(value) && hasString(value, "default_branch");
}

function isGhBranch(value: unknown): value is GhBranch {
	return isObject(value) && hasString(value, "name") && isCommitSha(value.commit);
}

function isGhObject(obj: unknown): obj is { readonly type: string; readonly sha: string } {
	return isObject(obj) && hasString(obj, "type") && hasString(obj, "sha");
}

function isGhRef(value: unknown): value is GhRef {
	return isObject(value) && hasString(value, "ref") && isGhObject(value.object);
}

function isGhTagObject(value: unknown): value is GhTagObject {
	return isObject(value) && isGhObject(value.object);
}

function isGhContent(value: unknown): value is GhContent {
	if (!isObject(value) || !hasString(value, "type")) return false;
	if (value.content !== undefined && typeof value.content !== "string") return false;
	if (value.encoding !== undefined && typeof value.encoding !== "string") return false;
	return true;
}

async function readJson(response: Response): Promise<unknown> {
	return await response.json();
}

function invalidShape(endpoint: string): GitHubProviderError {
	return new GitHubProviderError(`Unexpected response shape from GitHub ${endpoint}`, "network", 0);
}

// -- Fetch helpers --

/**
 * Execute a GET request and return the Response.
 * Throws {@link GitHubProviderError} on non-2xx responses.
 */
async function ghFetch(url: string, headers: Record<string, string>): Promise<Response> {
	const response = await fetch(url, { method: "GET", headers });

	if (response.ok) {
		warnRateLimit(response);
		return response;
	}

	// Rate limit: 429 always, 403 only with evidence of exhaustion
	const remaining = parseIntHeader(response, "x-ratelimit-remaining");
	const retryAfter = parseRetryAfter(response);
	const rateLimitEvidence = remaining === 0 || retryAfter !== undefined;

	if (response.status === 429 || (response.status === 403 && rateLimitEvidence)) {
		throw new GitHubProviderError(
			`GitHub API rate limited (${response.status})`,
			"rateLimited",
			response.status,
			{ rateLimitRemaining: remaining ?? undefined, retryAfter },
		);
	}

	// Forbidden (no rate-limit evidence)
	if (response.status === 403) {
		throw new GitHubProviderError(`GitHub API forbidden (403)`, "forbidden", 403);
	}

	// Auth
	if (response.status === 401) {
		throw new GitHubProviderError("GitHub API authentication failed (401)", "auth", 401);
	}

	// Not found
	if (response.status === 404) {
		throw new GitHubProviderError("GitHub API resource not found (404)", "notFound", 404);
	}

	// Other
	const body = await response.text().catch(() => "");
	throw new GitHubProviderError(
		`GitHub API error: ${response.status} ${response.statusText}${body ? ` — ${body.slice(0, 200)}` : ""}`,
		"network",
		response.status,
	);
}

/** Parse a `Retry-After` or `X-RateLimit-Reset` header into seconds. */
function parseRetryAfter(response: Response): number | undefined {
	// Retry-After: seconds
	const retryAfter = response.headers.get("retry-after");
	if (retryAfter !== null) {
		const secs = Number.parseInt(retryAfter, 10);
		if (Number.isFinite(secs) && secs > 0) return secs;
	}

	// X-RateLimit-Reset: unix timestamp
	const resetHeader = response.headers.get("x-ratelimit-reset");
	if (resetHeader !== null) {
		const resetAt = Number.parseInt(resetHeader, 10);
		if (Number.isFinite(resetAt)) {
			const delta = resetAt - Math.floor(Date.now() / 1000);
			if (delta > 0) return delta;
		}
	}

	return undefined;
}

/** Parse an integer header value, returning null if missing/invalid. */
function parseIntHeader(response: Response, name: string): number | null {
	const val = response.headers.get(name);
	if (val === null) return null;
	const n = Number.parseInt(val, 10);
	return Number.isFinite(n) ? n : null;
}

/** Warn (console) when rate limit remaining drops below threshold. */
function warnRateLimit(response: Response): void {
	const remaining = parseIntHeader(response, "x-ratelimit-remaining");
	if (remaining !== null && remaining < 10) {
		console.warn(`[actup-wasm] GitHub API rate limit low: ${remaining.toString()} remaining`);
	}
}

/**
 * Parse the `Link` header for the `rel="next"` URL.
 * Returns `undefined` if there is no next page.
 */
function parseNextLink(response: Response): string | undefined {
	const link = response.headers.get("link");
	if (link === null) return undefined;

	// Link: <url>; rel="next", <url>; rel="last"
	for (const part of link.split(",")) {
		if (part.includes("rel=\"next\"")) {
			const match = /<([^>]+)>/.exec(part);
			if (match?.[1] !== undefined) return match[1];
		}
	}

	return undefined;
}

/** Decode base64-encoded file content from GitHub Contents API. */
function decodeContent(content: string): string {
	// GitHub embeds newlines in base64 content
	const cleaned = content.replace(/\n/g, "");

	// Use atob (available everywhere: browsers, Node 16+, Bun, Deno)
	const binary = atob(cleaned);
	const bytes = new Uint8Array(binary.length);
	for (let i = 0; i < binary.length; i++) {
		// charCodeAt returns a number; the non-null assertion is safe here
		// because we iterate within bounds.
		bytes[i] = binary.charCodeAt(i);
	}
	return new TextDecoder().decode(bytes);
}

// -- Provider method implementations --

async function getVersions(
	apiBase: string,
	headers: Record<string, string>,
	perPage: number,
	owner: string,
	repo: string,
): Promise<VersionSet> {
	// Paginate through all tags
	const allTags: Array<{ readonly version: string; readonly sha: string }> = [];
	let url: string | undefined = `${apiBase}/repos/${owner}/${repo}/tags?per_page=${perPage.toString()}`;

	while (url !== undefined) {
		const response = await ghFetch(url, headers);
		const tags = await readJson(response);
		if (!isGhTagArray(tags)) throw invalidShape("/tags");
		for (const tag of tags) {
			allTags.push({ version: tag.name, sha: tag.commit.sha });
		}
		url = parseNextLink(response);
	}

	// Fetch default branch info. Rust deserializes `null` as `None`, so use
	// null (not undefined) when the call fails — matches the generated type.
	let defaultBranch: { readonly name: string; readonly sha: string } | null = null;
	try {
		const repoInfo = await readJson(await ghFetch(`${apiBase}/repos/${owner}/${repo}`, headers));
		if (!isGhRepo(repoInfo)) throw invalidShape("/repos/{owner}/{repo}");
		const branchName = repoInfo.default_branch;
		const branchInfo = await readJson(
			await ghFetch(`${apiBase}/repos/${owner}/${repo}/branches/${encodeURIComponent(branchName)}`, headers),
		);
		if (!isGhBranch(branchInfo)) throw invalidShape("/repos/{owner}/{repo}/branches/{branch}");
		defaultBranch = { name: branchInfo.name, sha: branchInfo.commit.sha };
	} catch {
		// Default branch info is supplementary; don't fail the whole call
	}

	return {
		semverTags: allTags,
		defaultBranch,
	};
}

async function resolveRefToSha(
	apiBase: string,
	headers: Record<string, string>,
	owner: string,
	repo: string,
	refName: string,
): Promise<string> {
	// Try as tag first
	try {
		const refData = await readJson(
			await ghFetch(`${apiBase}/repos/${owner}/${repo}/git/ref/tags/${encodeURIComponent(refName)}`, headers),
		);
		if (!isGhRef(refData)) throw invalidShape("/git/ref/tags/{ref}");

		// Lightweight tag: object is the commit directly
		if (refData.object.type === "commit") {
			return refData.object.sha;
		}

		// Annotated tag: dereference to get the commit
		if (refData.object.type === "tag") {
			const tagObj = await readJson(
				await ghFetch(`${apiBase}/repos/${owner}/${repo}/git/tags/${refData.object.sha}`, headers),
			);
			if (!isGhTagObject(tagObj)) throw invalidShape("/git/tags/{sha}");
			return tagObj.object.sha;
		}

		return refData.object.sha;
	} catch (err) {
		// 404 = not a tag, try as branch
		if (isNotFound(err)) return await resolveBranchOrCommit(apiBase, headers, owner, repo, refName);
		throw err;
	}
}

function isNotFound(err: unknown): boolean {
	return err instanceof GitHubProviderError && err.status === 404;
}

const SHORT_SHA = /^[0-9a-fA-F]{7,39}$/;

async function resolveBranchOrCommit(
	apiBase: string,
	headers: Record<string, string>,
	owner: string,
	repo: string,
	refName: string,
): Promise<string> {
	try {
		const branchInfo = await readJson(
			await ghFetch(`${apiBase}/repos/${owner}/${repo}/branches/${encodeURIComponent(refName)}`, headers),
		);
		if (!isGhBranch(branchInfo)) throw invalidShape("/branches/{branch}");
		return branchInfo.commit.sha;
	} catch (err) {
		if (!isNotFound(err) || !SHORT_SHA.test(refName)) throw err;
	}
	const commit = await readJson(await ghFetch(`${apiBase}/repos/${owner}/${repo}/commits/${refName}`, headers));
	if (!isCommitSha(commit)) throw invalidShape("/commits/{sha}");
	return commit.sha;
}

async function getDefaultBranch(
	apiBase: string,
	headers: Record<string, string>,
	owner: string,
	repo: string,
): Promise<string> {
	const repoInfo = await readJson(await ghFetch(`${apiBase}/repos/${owner}/${repo}`, headers));
	if (!isGhRepo(repoInfo)) throw invalidShape("/repos/{owner}/{repo}");
	return repoInfo.default_branch;
}

async function getReadme(
	apiBase: string,
	headers: Record<string, string>,
	owner: string,
	repo: string,
	ref: string | undefined,
): Promise<string | null> {
	const refParam = ref !== undefined ? `?ref=${encodeURIComponent(ref)}` : "";

	try {
		const content = await readJson(
			await ghFetch(`${apiBase}/repos/${owner}/${repo}/contents/README.md${refParam}`, headers),
		);
		if (!isGhContent(content)) throw invalidShape("/contents/README.md");
		if (content.type !== "file" || content.content === undefined) return null;
		if (content.encoding !== "base64") return null;
		return decodeContent(content.content);
	} catch (err) {
		if (err instanceof GitHubProviderError && err.status === 404) return null;
		throw err;
	}
}

async function getActionYml(
	apiBase: string,
	headers: Record<string, string>,
	owner: string,
	repo: string,
	ref: string,
): Promise<ActionYml> {
	const refParam = `?ref=${encodeURIComponent(ref)}`;

	// Try action.yml first, then action.yaml
	for (const filename of ["action.yml", "action.yaml"]) {
		try {
			const content = await readJson(
				await ghFetch(`${apiBase}/repos/${owner}/${repo}/contents/${filename}${refParam}`, headers),
			);
			if (!isGhContent(content)) throw invalidShape(`/contents/${filename}`);

			if (content.type !== "file" || content.content === undefined || content.encoding !== "base64") {
				continue;
			}

			const raw = decodeContent(content.content);
			return parseActionYmlMinimal(raw);
		} catch (err) {
			if (err instanceof GitHubProviderError && err.status === 404) continue;
			throw err;
		}
	}

	throw new GitHubProviderError(
		`Neither action.yml nor action.yaml found in ${owner}/${repo}@${ref}`,
		"notFound",
		404,
	);
}

/**
 * Minimal best-effort parse of action.yml content.
 *
 * This doesn't pull in a YAML parser dependency — it extracts top-level
 * scalar fields (name, description, author) and returns the raw content
 * for the WASM-side to do full parsing if needed.
 */
function parseActionYmlMinimal(raw: string): ActionYml {
	return {
		raw,
		name: extractYamlScalar(raw, "name"),
		description: extractYamlScalar(raw, "description"),
		author: extractYamlScalar(raw, "author"),
		inputs: {},
		outputs: {},
		runs: null,
	};
}

const MAX_YAML_INPUT = 1_000_000;

function escapeRegex(s: string): string {
	return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Extract a top-level YAML scalar value by key.
 * Handles both quoted and unquoted values. Returns null if not found.
 */
function extractYamlScalar(yaml: string, key: string): string | null {
	if (yaml.length > MAX_YAML_INPUT) return null;
	const pattern = new RegExp(
		`^${escapeRegex(key)}:\\s*(?:['"]([^'"\\r\\n]*)['"]|([^\\r\\n]*))\\s*$`,
		"m",
	);
	const match = pattern.exec(yaml);
	if (match === null) return null;
	return match[1] ?? match[2] ?? null;
}
