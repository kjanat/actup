/**
 * Default fetch()-based GitHub provider.
 *
 * Implements the {@link ActupProvider} interface using the GitHub REST API.
 * Works in browsers, Node.js 18+, Bun, Deno, and Cloudflare Workers — any
 * runtime with a global `fetch`.
 *
 * @module
 */

import type { ActionYml, ActupProvider, VersionSet } from "./types.ts";

/** Options for {@link createGitHubProvider}. */
export interface GitHubProviderOptions {
	/**
	 * Personal access token or fine-grained token for authentication.
	 * Without a token, requests are subject to 60 req/hr IP-based rate limits.
	 */
	readonly token?: string | undefined;
	/**
	 * API base URL. Defaults to `https://api.github.com`.
	 * For GitHub Enterprise, use `https://{host}/api/v3`.
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
	readonly status: number;
	readonly rateLimitRemaining: number | undefined;
	readonly retryAfter: number | undefined;

	constructor(
		message: string,
		status: number,
		options?: {
			readonly rateLimitRemaining?: number | undefined;
			readonly retryAfter?: number | undefined;
		},
	) {
		super(message);
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
	const apiBase = normalizeBase(options?.apiBase ?? "https://api.github.com");
	const perPage = clamp(options?.perPage ?? 100, 1, 100);
	const headers = buildHeaders(options?.token);

	return {
		getVersions: (host, owner, repo) => getVersions(apiBase, headers, perPage, owner, repo),
		resolveRefToSha: (host, owner, repo, ref) => resolveRefToSha(apiBase, headers, owner, repo, ref),
		getDefaultBranch: (host, owner, repo) => getDefaultBranch(apiBase, headers, owner, repo),
		getReadme: (host, owner, repo, ref) => getReadme(apiBase, headers, owner, repo, ref ?? undefined),
		getActionYml: (host, owner, repo, ref) => getActionYml(apiBase, headers, owner, repo, ref),
	};
}

// -- Implementation --

/** Build standard headers for GitHub REST API requests. */
function buildHeaders(token: string | undefined): Record<string, string> {
	const h: Record<string, string> = {
		"Accept": "application/vnd.github+json",
		"User-Agent": "actup-wasm/0.1",
		"X-GitHub-Api-Version": "2022-11-28",
	};
	if (token !== undefined && token.length > 0) {
		h["Authorization"] = `Bearer ${token}`;
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

	// Rate limit
	const remaining = parseIntHeader(response, "x-ratelimit-remaining");
	if (response.status === 403 || response.status === 429) {
		const retryAfter = parseRetryAfter(response);
		throw new GitHubProviderError(
			`GitHub API rate limited (${response.status})`,
			response.status,
			{ rateLimitRemaining: remaining ?? undefined, retryAfter },
		);
	}

	// Auth
	if (response.status === 401) {
		throw new GitHubProviderError("GitHub API authentication failed (401)", 401);
	}

	// Not found
	if (response.status === 404) {
		throw new GitHubProviderError("GitHub API resource not found (404)", 404);
	}

	// Other
	const body = await response.text().catch(() => "");
	throw new GitHubProviderError(
		`GitHub API error: ${response.status} ${response.statusText}${body ? ` — ${body.slice(0, 200)}` : ""}`,
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
		const tags: GhTag[] = (await response.json()) as GhTag[];
		for (const tag of tags) {
			allTags.push({ version: tag.name, sha: tag.commit.sha });
		}
		url = parseNextLink(response);
	}

	// Fetch default branch info
	let defaultBranch: { readonly name: string; readonly sha: string } | undefined;
	try {
		const repoInfo = (await (await ghFetch(`${apiBase}/repos/${owner}/${repo}`, headers)).json()) as GhRepo;
		const branchName = repoInfo.default_branch;
		const branchInfo = (await (
			await ghFetch(`${apiBase}/repos/${owner}/${repo}/branches/${encodeURIComponent(branchName)}`, headers)
		).json()) as GhBranch;
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
		const refData = (await (
			await ghFetch(`${apiBase}/repos/${owner}/${repo}/git/ref/tags/${encodeURIComponent(refName)}`, headers)
		).json()) as GhRef;

		// Lightweight tag: object is the commit directly
		if (refData.object.type === "commit") {
			return refData.object.sha;
		}

		// Annotated tag: dereference to get the commit
		if (refData.object.type === "tag") {
			const tagObj = (await (
				await ghFetch(`${apiBase}/repos/${owner}/${repo}/git/tags/${refData.object.sha}`, headers)
			).json()) as GhTagObject;
			return tagObj.object.sha;
		}

		return refData.object.sha;
	} catch (err) {
		// 404 = not a tag, try as branch
		if (err instanceof GitHubProviderError && err.status === 404) {
			const branchInfo = (await (
				await ghFetch(`${apiBase}/repos/${owner}/${repo}/branches/${encodeURIComponent(refName)}`, headers)
			).json()) as GhBranch;
			return branchInfo.commit.sha;
		}
		throw err;
	}
}

async function getDefaultBranch(
	apiBase: string,
	headers: Record<string, string>,
	owner: string,
	repo: string,
): Promise<string> {
	const repoInfo = (await (await ghFetch(`${apiBase}/repos/${owner}/${repo}`, headers)).json()) as GhRepo;
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
		const content = (await (
			await ghFetch(`${apiBase}/repos/${owner}/${repo}/contents/README.md${refParam}`, headers)
		).json()) as GhContent;

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
			const content = (await (
				await ghFetch(`${apiBase}/repos/${owner}/${repo}/contents/${filename}${refParam}`, headers)
			).json()) as GhContent;

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

/**
 * Extract a top-level YAML scalar value by key.
 * Handles both quoted and unquoted values. Returns null if not found.
 */
function extractYamlScalar(yaml: string, key: string): string | null {
	// Match `key:` at start of line, optional quotes around value
	const pattern = new RegExp(`^${key}:\\s*(?:['"]([^'"]*?)['"]|(.+?))\\s*$`, "m");
	const match = pattern.exec(yaml);
	if (match === null) return null;
	return match[1] ?? match[2] ?? null;
}
