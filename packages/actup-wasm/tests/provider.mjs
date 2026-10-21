// Unit test: verify createGitHubProvider against a mock HTTP server.
//
// Run after `bun run build`:
//   node packages/actup-wasm/tests/provider.mjs
//
// Uses Node.js built-in test runner (no deps) and a mock fetch via
// globalThis override.

import file from 'node:fs/promises';
import { resolve } from 'node:path';

// check if dist file exists
const providerPath = resolve(import.meta.dirname, '../dist/provider.js');
try {
	await file.access(providerPath);
} catch (err) {
	console.error(err.message, `\nPlease run \`bun run build\` first.`);
	process.exit(1);
}

const { createGitHubProvider, GitHubProviderError } = await import('../dist/provider.js');

const originalFetch = globalThis.fetch;
let fetchLog = [];

/** Install a mock fetch that dispatches based on URL patterns. */
function mockFetch(routes) {
	fetchLog = [];
	globalThis.fetch = async (url, init) => {
		fetchLog.push({ url, method: init?.method ?? 'GET', headers: init?.headers ?? {} });
		for (const route of routes) {
			if (route.match(url)) {
				return route.respond(url);
			}
		}
		return new Response(JSON.stringify({ message: 'Not Found' }), {
			status: 404,
			headers: { 'Content-Type': 'application/json' },
		});
	};
}

/** Restore real fetch. */
function restoreFetch() {
	globalThis.fetch = originalFetch;
	fetchLog = [];
}

/** Create a Response with JSON body and optional headers. */
function jsonResponse(body, status = 200, headers = {}) {
	return new Response(JSON.stringify(body), {
		status,
		headers: { 'Content-Type': 'application/json', ...headers },
	});
}

/** Base64-encode a string (matching GitHub Contents API format). */
function base64Encode(str) {
	return Buffer.from(str, 'utf-8').toString('base64');
}

function assert(cond, msg) {
	if (!cond) throw new Error(`Assertion failed: ${msg}`);
}

function assertEqual(actual, expected, msg) {
	if (actual !== expected) {
		throw new Error(`${msg}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
	}
}

let passed = 0;
let failed = 0;

async function test(name, fn) {
	try {
		await fn();
		passed++;
		console.log(`  PASS: ${name}`);
	} catch (err) {
		failed++;
		console.error(`  FAIL: ${name}`);
		console.error(`    ${err.message}`);
	} finally {
		restoreFetch();
	}
}

// -- Tests --

console.log('Provider tests\n');

// Test: getVersions returns tags + default branch
await test('getVersions returns semver tags and default branch', async () => {
	const tags = [
		{ name: 'v1.0.0', commit: { sha: 'a'.repeat(40) } },
		{ name: 'v1.1.0', commit: { sha: 'b'.repeat(40) } },
		{ name: 'v2.0.0', commit: { sha: 'c'.repeat(40) } },
	];

	mockFetch([
		{
			match: (url) => url.includes('/repos/actions/checkout/tags'),
			respond: () => jsonResponse(tags),
		},
		{
			match: (url) => url === 'https://api.github.com/repos/actions/checkout',
			respond: () => jsonResponse({ default_branch: 'main', full_name: 'actions/checkout', archived: false }),
		},
		{
			match: (url) => url.includes('/repos/actions/checkout/branches/main'),
			respond: () => jsonResponse({ name: 'main', commit: { sha: 'd'.repeat(40) } }),
		},
	]);

	const provider = createGitHubProvider();
	const result = await provider.getVersions('github.com', 'actions', 'checkout');

	assertEqual(result.semverTags.length, 3, 'semverTags count');
	assertEqual(result.semverTags[0].version, 'v1.0.0', 'first tag version');
	assertEqual(result.semverTags[0].sha, 'a'.repeat(40), 'first tag sha');
	assertEqual(result.semverTags[2].version, 'v2.0.0', 'third tag version');
	assert(result.defaultBranch !== undefined, 'defaultBranch present');
	assertEqual(result.defaultBranch.name, 'main', 'defaultBranch name');
	assertEqual(result.defaultBranch.sha, 'd'.repeat(40), 'defaultBranch sha');
});

// Test: getVersions pagination
await test('getVersions follows pagination', async () => {
	const page1 = [{ name: 'v1.0.0', commit: { sha: 'a'.repeat(40) } }];
	const page2 = [{ name: 'v2.0.0', commit: { sha: 'b'.repeat(40) } }];

	mockFetch([
		{
			match: (url) => url.includes('/tags?per_page=') && !url.includes('page=2'),
			respond: () =>
				jsonResponse(page1, 200, {
					Link: '<https://api.github.com/repos/o/r/tags?per_page=100&page=2>; rel="next"',
				}),
		},
		{
			match: (url) => url.includes('page=2'),
			respond: () => jsonResponse(page2),
		},
		{
			match: (url) => url.endsWith('/repos/o/r'),
			respond: () => jsonResponse({ default_branch: 'main' }),
		},
		{
			match: (url) => url.includes('/branches/main'),
			respond: () => jsonResponse({ name: 'main', commit: { sha: 'c'.repeat(40) } }),
		},
	]);

	const provider = createGitHubProvider();
	const result = await provider.getVersions('github.com', 'o', 'r');

	assertEqual(result.semverTags.length, 2, 'got both pages');
	assertEqual(result.semverTags[0].version, 'v1.0.0', 'page 1 tag');
	assertEqual(result.semverTags[1].version, 'v2.0.0', 'page 2 tag');
});

// Test: resolveRefToSha — lightweight tag
await test('resolveRefToSha resolves lightweight tag', async () => {
	mockFetch([
		{
			match: (url) => url.includes('/git/ref/tags/v1.0.0'),
			respond: () => jsonResponse({ ref: 'refs/tags/v1.0.0', object: { type: 'commit', sha: 'a'.repeat(40) } }),
		},
	]);

	const provider = createGitHubProvider();
	const sha = await provider.resolveRefToSha('github.com', 'actions', 'checkout', 'v1.0.0');
	assertEqual(sha, 'a'.repeat(40), 'sha');
});

// Test: resolveRefToSha — annotated tag (dereference)
await test('resolveRefToSha dereferences annotated tag', async () => {
	const tagObjSha = 'f'.repeat(40);
	const commitSha = 'a'.repeat(40);

	mockFetch([
		{
			match: (url) => url.includes('/git/ref/tags/v2.0.0'),
			respond: () => jsonResponse({ ref: 'refs/tags/v2.0.0', object: { type: 'tag', sha: tagObjSha } }),
		},
		{
			match: (url) => url.includes(`/git/tags/${tagObjSha}`),
			respond: () => jsonResponse({ tag: 'v2.0.0', message: 'release', object: { type: 'commit', sha: commitSha } }),
		},
	]);

	const provider = createGitHubProvider();
	const sha = await provider.resolveRefToSha('github.com', 'actions', 'checkout', 'v2.0.0');
	assertEqual(sha, commitSha, 'dereferenced sha');
});

// Test: resolveRefToSha — falls back to branch
await test('resolveRefToSha falls back to branch on 404', async () => {
	mockFetch([
		{
			match: (url) => url.includes('/git/ref/tags/main'),
			respond: () => jsonResponse({ message: 'Not Found' }, 404),
		},
		{
			match: (url) => url.includes('/branches/main'),
			respond: () => jsonResponse({ name: 'main', commit: { sha: 'b'.repeat(40) } }),
		},
	]);

	const provider = createGitHubProvider();
	const sha = await provider.resolveRefToSha('github.com', 'actions', 'checkout', 'main');
	assertEqual(sha, 'b'.repeat(40), 'branch sha');
});

await test('resolveRefToSha falls back to the commit for a short SHA', async () => {
	mockFetch([
		{
			match: (url) => url.endsWith('/repos/o/r/commits/abc1234'),
			respond: () => jsonResponse({ sha: 'abc1234'.padEnd(40, '0') }),
		},
	]);

	const provider = createGitHubProvider();
	const sha = await provider.resolveRefToSha('github.com', 'o', 'r', 'abc1234');
	assertEqual(sha, 'abc1234'.padEnd(40, '0'), 'commit sha');
	assertEqual(
		fetchLog.map((f) => new URL(f.url).pathname).join(' '),
		'/repos/o/r/git/ref/tags/abc1234 /repos/o/r/branches/abc1234 /repos/o/r/commits/abc1234',
		'tag, branch, then commit',
	);
});

await test('resolveRefToSha prefers a hex-named branch over the commit', async () => {
	mockFetch([
		{
			match: (url) => url.endsWith('/branches/deadbee'),
			respond: () => jsonResponse({ name: 'deadbee', commit: { sha: 'b'.repeat(40) } }),
		},
	]);

	const provider = createGitHubProvider();
	const sha = await provider.resolveRefToSha('github.com', 'o', 'r', 'deadbee');
	assertEqual(sha, 'b'.repeat(40), 'branch sha');
	assert(fetchLog.every((f) => !f.url.includes('/commits/')), 'no commit lookup');
});

await test('resolveRefToSha skips the commit lookup for a non-hex name', async () => {
	mockFetch([]);

	const provider = createGitHubProvider();
	try {
		await provider.resolveRefToSha('github.com', 'o', 'r', 'nope');
		throw new Error('Should have thrown');
	} catch (err) {
		assert(err instanceof GitHubProviderError, 'GitHubProviderError');
		assertEqual(err.status, 404, 'status 404');
	}
	assert(fetchLog.every((f) => !f.url.includes('/commits/')), 'no commit lookup');
});

// Test: getDefaultBranch
await test('getDefaultBranch returns repo default', async () => {
	mockFetch([
		{
			match: (url) => url.endsWith('/repos/actions/checkout'),
			respond: () => jsonResponse({ default_branch: 'main' }),
		},
	]);

	const provider = createGitHubProvider();
	const branch = await provider.getDefaultBranch('github.com', 'actions', 'checkout');
	assertEqual(branch, 'main', 'branch name');
});

// Test: getReadme
await test('getReadme returns decoded content', async () => {
	const readmeText = '# Hello\n\nThis is a readme.';
	mockFetch([
		{
			match: (url) => url.includes('/contents/README.md'),
			respond: () =>
				jsonResponse({
					type: 'file',
					content: base64Encode(readmeText),
					encoding: 'base64',
					name: 'README.md',
					path: 'README.md',
					sha: 'e'.repeat(40),
				}),
		},
	]);

	const provider = createGitHubProvider();
	const readme = await provider.getReadme('github.com', 'actions', 'checkout', 'v4');
	assertEqual(readme, readmeText, 'decoded readme');
});

// Test: getReadme — 404 returns null
await test('getReadme returns null on 404', async () => {
	mockFetch([
		{
			match: (url) => url.includes('/contents/README.md'),
			respond: () => jsonResponse({ message: 'Not Found' }, 404),
		},
	]);

	const provider = createGitHubProvider();
	const readme = await provider.getReadme('github.com', 'actions', 'checkout', 'v4');
	assertEqual(readme, null, 'null on 404');
});

// Test: getReadme with no ref
await test('getReadme omits ref param when not provided', async () => {
	mockFetch([
		{
			match: (url) => url.includes('/contents/README.md') && !url.includes('?ref='),
			respond: () =>
				jsonResponse({
					type: 'file',
					content: base64Encode('# Readme'),
					encoding: 'base64',
				}),
		},
	]);

	const provider = createGitHubProvider();
	const readme = await provider.getReadme('github.com', 'actions', 'checkout');
	assertEqual(readme, '# Readme', 'readme without ref');
});

// Test: getActionYml — action.yml exists
await test('getActionYml returns parsed action.yml', async () => {
	const actionContent = `name: Checkout\ndescription: Check out a repo\nauthor: GitHub\nruns:\n  using: node20\n  main: index.js\n`;
	mockFetch([
		{
			match: (url) => url.includes('/contents/action.yml'),
			respond: () =>
				jsonResponse({
					type: 'file',
					content: base64Encode(actionContent),
					encoding: 'base64',
				}),
		},
	]);

	const provider = createGitHubProvider();
	const yml = await provider.getActionYml('github.com', 'actions', 'checkout', 'v4');
	assertEqual(yml.raw, actionContent, 'raw content');
	assertEqual(yml.name, 'Checkout', 'name');
	assertEqual(yml.description, 'Check out a repo', 'description');
	assertEqual(yml.author, 'GitHub', 'author');
});

// Test: getActionYml — falls back to action.yaml
await test('getActionYml falls back to action.yaml', async () => {
	const actionContent = `name: Alt\ndescription: Alternative\n`;
	mockFetch([
		{
			match: (url) => url.includes('/contents/action.yml'),
			respond: () => jsonResponse({ message: 'Not Found' }, 404),
		},
		{
			match: (url) => url.includes('/contents/action.yaml'),
			respond: () =>
				jsonResponse({
					type: 'file',
					content: base64Encode(actionContent),
					encoding: 'base64',
				}),
		},
	]);

	const provider = createGitHubProvider();
	const yml = await provider.getActionYml('github.com', 'actions', 'checkout', 'v4');
	assertEqual(yml.name, 'Alt', 'fallback name');
});

// Test: getActionYml — neither exists
await test('getActionYml throws when neither exists', async () => {
	mockFetch([
		{
			match: (url) => url.includes('/contents/action.yml') || url.includes('/contents/action.yaml'),
			respond: () => jsonResponse({ message: 'Not Found' }, 404),
		},
	]);

	const provider = createGitHubProvider();
	try {
		await provider.getActionYml('github.com', 'actions', 'checkout', 'v4');
		throw new Error('Should have thrown');
	} catch (err) {
		assert(err instanceof GitHubProviderError, 'GitHubProviderError instance');
		assertEqual(err.status, 404, 'status 404');
		assertEqual(err.kind, 'notFound', 'kind notFound');
		assert(err.message.includes('Neither action.yml nor action.yaml'), 'message mentions both files');
	}
});

// Test: auth header sent when token provided
await test('sends Authorization header with token', async () => {
	mockFetch([
		{
			match: (url) => url.endsWith('/repos/o/r'),
			respond: () => jsonResponse({ default_branch: 'main' }),
		},
	]);

	const provider = createGitHubProvider({ token: 'ghp_test123' });
	await provider.getDefaultBranch('github.com', 'o', 'r');

	assertEqual(fetchLog.length, 1, 'one fetch call');
	const sent = fetchLog[0].headers;
	const authValue = sent instanceof Headers ? sent.get('Authorization') : sent.Authorization;
	assertEqual(authValue, 'Bearer ghp_test123', 'Authorization header present and correct');
});

// Test: custom apiBase
await test('uses custom apiBase', async () => {
	mockFetch([
		{
			match: (url) => url.startsWith('https://ghe.example.com/api/v3/repos/o/r'),
			respond: () => jsonResponse({ default_branch: 'develop' }),
		},
	]);

	const provider = createGitHubProvider({ apiBase: 'https://ghe.example.com/api/v3' });
	const branch = await provider.getDefaultBranch('ghe.example.com', 'o', 'r');
	assertEqual(branch, 'develop', 'custom apiBase branch');
});

// Test: rate limit error
await test('throws GitHubProviderError on rate limit (429)', async () => {
	mockFetch([
		{
			match: () => true,
			respond: () =>
				jsonResponse({ message: 'rate limit exceeded' }, 429, {
					'retry-after': '60',
					'x-ratelimit-remaining': '0',
				}),
		},
	]);

	const provider = createGitHubProvider();
	try {
		await provider.getDefaultBranch('github.com', 'o', 'r');
		throw new Error('Should have thrown');
	} catch (err) {
		assert(err instanceof GitHubProviderError, 'GitHubProviderError');
		assertEqual(err.status, 429, 'status 429');
		assertEqual(err.kind, 'rateLimited', 'kind rateLimited');
		assertEqual(err.retryAfter, 60, 'retryAfter from header');
		assertEqual(err.rateLimitRemaining, 0, 'remaining 0');
	}
});

// Test: rate limit error (403 variant)
await test('throws GitHubProviderError on rate limit (403)', async () => {
	mockFetch([
		{
			match: () => true,
			respond: () =>
				jsonResponse({ message: 'API rate limit exceeded' }, 403, {
					'x-ratelimit-remaining': '0',
					'x-ratelimit-reset': String(Math.floor(Date.now() / 1000) + 120),
				}),
		},
	]);

	const provider = createGitHubProvider();
	try {
		await provider.getDefaultBranch('github.com', 'o', 'r');
		throw new Error('Should have thrown');
	} catch (err) {
		assert(err instanceof GitHubProviderError, 'GitHubProviderError');
		assertEqual(err.status, 403, 'status 403');
		assertEqual(err.kind, 'rateLimited', 'kind rateLimited');
		assert(err.retryAfter !== undefined && err.retryAfter > 0, 'retryAfter from reset header');
	}
});

// Test: auth error
await test('throws GitHubProviderError on 401', async () => {
	mockFetch([
		{
			match: () => true,
			respond: () => jsonResponse({ message: 'Bad credentials' }, 401),
		},
	]);

	const provider = createGitHubProvider({ token: 'bad-token' });
	try {
		await provider.getDefaultBranch('github.com', 'o', 'r');
		throw new Error('Should have thrown');
	} catch (err) {
		assert(err instanceof GitHubProviderError, 'GitHubProviderError');
		assertEqual(err.status, 401, 'status 401');
		assertEqual(err.kind, 'auth', 'kind auth');
	}
});

await test('sets kind from 403, 404 and 500 statuses', async () => {
	for (const [status, kind] of [[403, 'forbidden'], [404, 'notFound'], [500, 'network']]) {
		mockFetch([{ match: () => true, respond: () => jsonResponse({ message: 'x' }, status) }]);
		try {
			await createGitHubProvider().getVersions('github.com', 'o', 'r');
			throw new Error('Should have thrown');
		} catch (err) {
			assert(err instanceof GitHubProviderError, `GitHubProviderError for ${status}`);
			assertEqual(err.kind, kind, `kind for ${status}`);
		}
	}
});

await test('sets kind network for an unexpected tag listing', async () => {
	mockFetch([{ match: () => true, respond: () => jsonResponse({ not: 'a list' }) }]);
	try {
		await createGitHubProvider().getVersions('github.com', 'o', 'r');
		throw new Error('Should have thrown');
	} catch (err) {
		assert(err instanceof GitHubProviderError, 'GitHubProviderError');
		assertEqual(err.kind, 'network', 'kind network');
	}
});

// Test: rate limit warning on low remaining (doesn't throw)
await test('warns on low rate limit remaining but succeeds', async () => {
	const warnings = [];
	const origWarn = console.warn;
	console.warn = (...args) => warnings.push(args.join(' '));

	try {
		mockFetch([
			{
				match: () => true,
				respond: () =>
					jsonResponse({ default_branch: 'main' }, 200, {
						'x-ratelimit-remaining': '5',
					}),
			},
		]);

		const provider = createGitHubProvider();
		const branch = await provider.getDefaultBranch('github.com', 'o', 'r');

		assertEqual(branch, 'main', 'still succeeds');
		assert(warnings.length > 0, 'warning emitted');
		assert(warnings[0].includes('rate limit low'), 'warning mentions rate limit');
	} finally {
		console.warn = origWarn;
	}
});

// Test: getVersions gracefully handles default branch failure
await test('getVersions returns tags even if default branch fetch fails', async () => {
	mockFetch([
		{
			match: (url) => url.includes('/tags'),
			respond: () => jsonResponse([{ name: 'v1.0.0', commit: { sha: 'a'.repeat(40) } }]),
		},
		{
			match: (url) => url.endsWith('/repos/o/r'),
			respond: () => jsonResponse({ message: 'Server Error' }, 500),
		},
	]);

	const provider = createGitHubProvider();
	const result = await provider.getVersions('github.com', 'o', 'r');
	assertEqual(result.semverTags.length, 1, 'got tags despite branch failure');
	assertEqual(result.defaultBranch, null, 'no defaultBranch on failure');
});

// Test: perPage option is respected
await test('perPage option clamped to valid range', async () => {
	mockFetch([
		{
			match: (url) => url.includes('per_page=10'),
			respond: () => jsonResponse([]),
		},
		{
			match: (url) => url.endsWith('/repos/o/r'),
			respond: () => jsonResponse({ default_branch: 'main' }),
		},
		{
			match: (url) => url.includes('/branches/main'),
			respond: () => jsonResponse({ name: 'main', commit: { sha: 'a'.repeat(40) } }),
		},
	]);

	const provider = createGitHubProvider({ perPage: 10 });
	const result = await provider.getVersions('github.com', 'o', 'r');
	assert(Array.isArray(result.semverTags), 'returns array');
	// Check that per_page=10 was in the URL
	assert(fetchLog[0].url.includes('per_page=10'), 'per_page=10 in URL');
});

// Test: multi-host derivation when apiBase is not configured
await test('derives apiBase from host when apiBase not configured', async () => {
	mockFetch([
		{
			match: (url) => url.startsWith('https://api.github.com/repos/o/r'),
			respond: () => jsonResponse({ default_branch: 'main' }),
		},
		{
			match: (url) => url.startsWith('https://ghe.example.com/api/v3/repos/o/r'),
			respond: () => jsonResponse({ default_branch: 'develop' }),
		},
	]);

	const provider = createGitHubProvider();
	// Same provider, two different hosts → two different backends.
	const ghBranch = await provider.getDefaultBranch('github.com', 'o', 'r');
	assertEqual(ghBranch, 'main', 'github.com routes to api.github.com');

	const gheBranch = await provider.getDefaultBranch('ghe.example.com', 'o', 'r');
	assertEqual(gheBranch, 'develop', 'GHE host routes to https://{host}/api/v3');
});

// Test: configured apiBase wins over per-call host (single-host mode)
await test('configured apiBase overrides host argument', async () => {
	mockFetch([
		{
			match: (url) => url.startsWith('https://my-ghe.corp/api/v3/repos/o/r'),
			respond: () => jsonResponse({ default_branch: 'main' }),
		},
	]);

	const provider = createGitHubProvider({ apiBase: 'https://my-ghe.corp/api/v3' });
	// host says github.com but configured base wins.
	const branch = await provider.getDefaultBranch('github.com', 'o', 'r');
	assertEqual(branch, 'main', 'configured apiBase used regardless of host');
});

// Test: empty host falls back to api.github.com
await test('empty host falls back to api.github.com', async () => {
	mockFetch([
		{
			match: (url) => url.startsWith('https://api.github.com/repos/o/r'),
			respond: () => jsonResponse({ default_branch: 'main' }),
		},
	]);

	const provider = createGitHubProvider();
	const branch = await provider.getDefaultBranch('', 'o', 'r');
	assertEqual(branch, 'main', 'empty host treated as github.com');
});

// Test: trailing slash in apiBase is handled
await test('apiBase trailing slash is stripped', async () => {
	mockFetch([
		{
			match: (url) => url.startsWith('https://api.github.com/repos/'),
			respond: () => jsonResponse({ default_branch: 'main' }),
		},
	]);

	const provider = createGitHubProvider({ apiBase: 'https://api.github.com/' });
	await provider.getDefaultBranch('github.com', 'o', 'r');
	assert(!fetchLog[0].url.includes('//repos'), 'no double slash');
});

// Test: dot segments never reach fetch
await test('dot segments are refused before any request', async () => {
	mockFetch([]);
	const provider = createGitHubProvider();
	for (
		const [owner, repo, ref] of [
			['o', 'r', '..'],
			['o', 'r', '.'],
			['..', 'r', 'v1'],
			['o', '..', 'v1'],
		]
	) {
		let error;
		try {
			await provider.resolveRefToSha('github.com', owner, repo, ref);
		} catch (err) {
			error = err;
		}
		assert(error instanceof GitHubProviderError, `${owner}/${repo}@${ref} throws`);
	}
	assertEqual(fetchLog.length, 0, 'no request');
});

// -- Summary --
console.log(`\n${passed} passed, ${failed} failed`);
if (failed > 0) {
	process.exit(1);
}
