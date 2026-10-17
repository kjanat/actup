// Integration test: verify the npm package works from Node.js.
//
// Run after `bun run build`:
//   node packages/actup-wasm/tests/node_import.mjs
import assert from "node:assert/strict";
import file from "node:fs/promises";
import { resolve } from "node:path";

// check if dist file exists
const indexPath = resolve(import.meta.dirname, "../dist/index.js");
try {
	await file.access(indexPath);
} catch (err) {
	console.error(err.message, `\nPlease run \`bun run build\` first.`);
	process.exit(1);
}

const { checkRefs, initWasm, isHostSegment, parseActionPath, parseUses, parseUsesRefs, planUpdates, sortVersionTags } = await import(
	"../dist/index.js"
);

// Test 1: initWasm + parseUsesRefs
await initWasm();

const yaml = `\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: "actions/setup-node@v3"
      - uses: './local/action'
`;

const locations = parseUsesRefs(yaml);

if (!Array.isArray(locations)) {
	throw new Error(`Expected array, got ${typeof locations}`);
}

if (locations.length !== 3) {
	throw new Error(`Expected 3 locations, got ${locations.length}`);
}

const first = locations[0];
if (first.value !== "actions/checkout@v4") {
	throw new Error(`Expected "actions/checkout@v4", got "${first.value}"`);
}
if (first.line !== 7) {
	throw new Error(`Expected line 7, got ${first.line}`);
}
assert.deepEqual(first.value_range, { start: 14, end: 33 }, "value range covers the value");
assert.deepEqual(locations[1].value_range, { start: 15, end: 36 }, "value range of a quoted value skips the quotes");

console.log("PASS: parseUsesRefs returned", locations.length, "locations");
console.log("  [0]", JSON.stringify(first));

// Test 2: double init is a no-op
await initWasm();
console.log("PASS: double initWasm is a no-op");

// Test 3: checkRefs with mock provider
const mockProvider = {
	async getVersions(_host, owner, repo) {
		if (owner === "actions" && repo === "checkout") {
			return {
				semverTags: [
					{ version: "v4.0.0", sha: "a".repeat(40) },
					{ version: "v4.2.0", sha: "b".repeat(40) },
				],
			};
		}
		if (owner === "actions" && repo === "setup-node") {
			return {
				semverTags: [
					{ version: "v3.0.0", sha: "c".repeat(40) },
					{ version: "v3.1.0", sha: "d".repeat(40) },
				],
			};
		}
		throw new Error(`Unknown: ${owner}/${repo}`);
	},
	async resolveRefToSha() {
		return "a".repeat(40);
	},
	async getDefaultBranch() {
		return "main";
	},
	async getReadme() {
		return null;
	},
	async getActionYml() {
		return { raw: "", name: null, description: null, author: null, inputs: {}, outputs: {}, runs: null };
	},
};

const results = await checkRefs(yaml, mockProvider);

assert.ok(Array.isArray(results), `Expected array, got ${typeof results}`);
assert.equal(results.length, 2, "checkRefs result count (skips local ./)");

const validStatuses = new Set(["UpToDate", "Outdated", "Unresolvable", "Unknown", "RateLimited"]);
// `CheckStatus` serializes unit variants as a string ("UpToDate") and struct
// variants as a single-key object ({ Outdated: {...} }) under serde's default
// externally-tagged representation. Normalize both shapes to the variant name.
const statusName = (s) => (typeof s === "string" ? s : Object.keys(s)[0]);

const checkoutResult = results.find((r) => r.action_ref?.repo === "checkout");
assert.ok(checkoutResult, "checkout result present");
assert.equal(checkoutResult.action_ref.owner, "actions", "checkout owner");
assert.equal(checkoutResult.action_ref.host, null, "absent host is null");
assert.equal(checkoutResult.action_ref.subpath, null, "absent subpath is null");
assert.deepEqual(checkoutResult.location, locations[0], "result carries its location");
assert.ok(
	validStatuses.has(statusName(checkoutResult.status)),
	`checkout status valid: got ${JSON.stringify(checkoutResult.status)}`,
);

const setupNodeResult = results.find((r) => r.action_ref?.repo === "setup-node");
assert.ok(setupNodeResult, "setup-node result present");
assert.equal(setupNodeResult.action_ref.owner, "actions", "setup-node owner");
assert.deepEqual(setupNodeResult.location, locations[1], "result carries its location");
assert.ok(
	validStatuses.has(statusName(setupNodeResult.status)),
	`setup-node status valid: got ${JSON.stringify(setupNodeResult.status)}`,
);

console.log("PASS: checkRefs returned", results.length, "results");
for (const r of results) {
	console.log("  ", JSON.stringify(r.action_ref), "->", JSON.stringify(r.status));
}

// Test 4: planUpdates
const edits = planUpdates(results);
assert.ok(Array.isArray(edits), "edits is array");
for (const e of edits) {
	assert.equal(typeof e.line, "number", "edit.line is number");
	assert.equal(typeof e.original, "string", "edit.original is string");
	assert.equal(typeof e.newRef, "string", "edit.newRef is string");
	assert.ok(e.line > 0, "edit.line positive");
}
console.log("PASS: planUpdates returned", edits.length, "edits");
for (const e of edits) {
	console.log("  line", e.line, ":", e.original, "->", e.newRef);
}

// Test 5: empty input
const empty = parseUsesRefs("");
if (empty.length !== 0) {
	throw new Error(`Expected 0 for empty, got ${empty.length}`);
}
console.log("PASS: empty YAML returns empty array");

// Test 6: end-to-end Outdated case — the wire shape consumers actually display.
//
// Existing tests use major-only refs (`@v4`) which resolve as UpToDate because
// the target major matches. To force Outdated we pin the YAML to a specific
// older patch and let the provider expose a newer one in the same major.
//
// This guards against the class of bug where `target.version` was a structural
// enum object instead of a display string — every diagnostic UI would render
// `[object Object]` if that regressed.
const outdatedYaml = `\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4.0.0
`;

const outdatedResults = await checkRefs(outdatedYaml, mockProvider);
assert.equal(outdatedResults.length, 1, "exactly one ref in outdated YAML");

const result = outdatedResults[0];
assert.equal(statusName(result.status), "Outdated", `expected Outdated, got ${statusName(result.status)}`);

// Wire shape consumers depend on: target.version is a string (Display form),
// target.sha is a 40-char hex string, current is the original YAML ref text.
assert.equal(typeof result.status.Outdated.current, "string", "Outdated.current is a string");
assert.equal(result.status.Outdated.current, "v4.0.0", "current echoes the YAML ref");

const target = result.status.Outdated.target;
assert.equal(typeof target.version, "string", "target.version is a string (not the Version enum object)");
assert.equal(target.version, "v4.2.0", "target.version is the latest tag's display form");
assert.equal(typeof target.sha, "string", "target.sha is a string");
assert.equal(target.sha.length, 40, "target.sha is 40 chars");

// planUpdates should emit one edit pointing at v4.2.0.
const outdatedEdits = planUpdates(outdatedResults);
assert.equal(outdatedEdits.length, 1, "one edit emitted");
assert.equal(outdatedEdits[0].newRef, "v4.2.0", "edit replaces with target version");
assert.equal(outdatedEdits[0].original, "actions/checkout@v4.0.0", "edit preserves the original uses string");

console.log("PASS: Outdated wire shape — current 'v4.0.0' -> target.version 'v4.2.0'");

// Test 7: `policy.bump` in the config caps the bump level.
const majorProvider = {
	...mockProvider,
	async getVersions() {
		return {
			semverTags: [
				{ version: "v4.0.0", sha: "a".repeat(40) },
				{ version: "v5.0.0", sha: "e".repeat(40) },
			],
		};
	},
};
const [latestResult] = await checkRefs(outdatedYaml, majorProvider);
assert.equal(statusName(latestResult.status), "Outdated", "default bump allows a new major");
assert.equal(latestResult.status.Outdated.target.version, "v5.0.0", "default bump targets v5.0.0");

const [minorResult] = await checkRefs(outdatedYaml, majorProvider, { policy: { bump: "minor" } });
assert.equal(statusName(minorResult.status), "UpToDate", "policy.bump = minor stays on v4");

console.log("PASS: config policy.bump = minor keeps v4.0.0 up to date");

// Test 8: unknown config keys are rejected.
await assert.rejects(checkRefs(outdatedYaml, majorProvider, { update_policy: "patch" }), /update_policy/);
await assert.rejects(checkRefs(outdatedYaml, majorProvider, { policy: { track: "exact" } }), /track/);
assert.throws(() => planUpdates(outdatedResults, { policy: { track: "exact" } }), /track/);

console.log("PASS: unknown config keys are rejected");

// Test 9: configs without a JSON form are rejected.
await assert.rejects(checkRefs(outdatedYaml, majorProvider, () => {}), /invalid config: expected an object/);
await assert.rejects(
	checkRefs(outdatedYaml, majorProvider, Symbol("config")),
	/invalid config: expected an object/,
);

console.log("PASS: function and Symbol configs are rejected");

// Test 10: array configs are rejected.
await assert.rejects(checkRefs(outdatedYaml, majorProvider, []), /invalid config: expected an object/);
await assert.rejects(
	checkRefs(outdatedYaml, majorProvider, ["ghe.corp"]),
	/invalid config: expected an object/,
);

console.log("PASS: array configs are rejected");

// Test 11: configs nested past serde_json's 128-level recursion limit are rejected.
let deep = [];
for (let i = 0; i < 200; i++) {
	deep = [deep];
}
await assert.rejects(
	checkRefs(outdatedYaml, majorProvider, { default_host: deep }),
	/invalid config: recursion limit exceeded/,
);

console.log("PASS: deeply nested configs are rejected");

// Test 12: a mixed-case host resolves on the default host and keeps its edit.
const hostYaml = `\
jobs:
  build:
    steps:
      - uses: GitHub.COM/actions/checkout@v4.0.0
`;
const hostResults = await checkRefs(hostYaml, mockProvider);
assert.equal(hostResults.length, 1, "one ref in host YAML");
assert.equal(hostResults[0].action_ref.host, "github.com", "host is lower-cased");
assert.equal(statusName(hostResults[0].status), "Outdated", `got ${JSON.stringify(hostResults[0].status)}`);
const hostEdits = planUpdates(hostResults);
assert.equal(hostEdits.length, 1, "one edit for the mixed-case host");
assert.equal(hostEdits[0].original, "GitHub.COM/actions/checkout@v4.0.0", "edit keeps the host as written");
assert.equal(hostEdits[0].newRef, "v4.2.0", "edit targets v4.2.0");

console.log("PASS: mixed-case host resolves and plans its edit");

// Test 13: a value the line does not hold verbatim gets no ranges and no edit.
const escapedYaml = `\
jobs:
  build:
    steps:
      - uses: "actions/checkout@v4.0.\\x30"
`;
const [escapedLoc] = parseUsesRefs(escapedYaml);
assert.equal(escapedLoc.value, "actions/checkout@v4.0.0", "escape is decoded");
assert.equal(escapedLoc.ref_range, null, "no ref range for an escaped value");
assert.equal(escapedLoc.value_range, null, "no value range for an escaped value");
const escapedResults = await checkRefs(escapedYaml, mockProvider);
assert.equal(statusName(escapedResults[0].status), "Outdated", "escaped ref is still checked");
assert.deepEqual(planUpdates(escapedResults), [], "no edit for an escaped value");

console.log("PASS: escaped value is checked but gets no edit");

// Test 14: a ref written twice gets one edit per location.
const repeatedYaml = `\
jobs:
  build:
    steps:
      - uses: actions/checkout@v4.0.0
      - uses: actions/checkout@v4.0.0
`;
const repeatedResults = await checkRefs(repeatedYaml, mockProvider);
assert.equal(repeatedResults.length, 2, "one result per location");
const repeatedEdits = planUpdates(repeatedResults);
assert.deepEqual(
	repeatedEdits.map((e) => [e.line, e.refStart, e.refEnd, e.newRef]),
	[
		[4, 31, 37, "v4.2.0"],
		[5, 31, 37, "v4.2.0"],
	],
	"one edit for each location",
);

console.log("PASS: repeated ref gets one edit per location");

// Test 15: `policy.pin` picks the written form of each outdated ref.
const oldSha = "a".repeat(40);
const newSha = "b".repeat(40);
const pinYaml = `\
jobs:
  build:
    steps:
      - uses: actions/checkout@v4.0.0
      - uses: actions/checkout@${oldSha} # v4.0.0
      - uses: actions/checkout@${oldSha}
`;
const pinResults = await checkRefs(pinYaml, mockProvider);
assert.deepEqual(pinResults.map((r) => statusName(r.status)), ["Outdated", "Outdated", "Outdated"]);
const applyEdits = (text, edits) => {
	const lines = text.split("\n");
	for (const e of edits) {
		const l = lines[e.line - 1];
		lines[e.line - 1] = l.slice(0, e.refStart) + e.newText + l.slice(e.replaceEnd);
	}
	return lines.join("\n");
};
const pinned = (refs) => `jobs:\n  build:\n    steps:\n${refs.map((r) => `      - uses: actions/checkout@${r}\n`).join("")}`;
const planned = (config) => applyEdits(pinYaml, planUpdates(pinResults, config));
assert.deepEqual(
	planUpdates(pinResults).map((e) => e.line),
	[4, 5, 6],
	"one edit per location when two locations share a ref",
);

assert.equal(
	planned(undefined),
	pinned(["v4.2.0", `${newSha} # v4.2.0`, `${newSha} # v4.2.0`]),
	"default pin = preserve keeps each ref style",
);
assert.equal(
	planned({ policy: { pin: "sha" } }),
	pinned([`${newSha} # v4.2.0`, `${newSha} # v4.2.0`, `${newSha} # v4.2.0`]),
	"pin = sha writes SHAs with version comments",
);
assert.equal(
	planned({ policy: { pin: "tag" } }),
	pinned(["v4.2.0", "v4.2.0", "v4.2.0"]),
	"pin = tag unpins outdated SHA pins",
);
const [shaEdit] = planUpdates(pinResults, { policy: { pin: "sha" } });
assert.equal(shaEdit.newRef, newSha, "newRef is the bare SHA");
assert.equal(shaEdit.replaceEnd, shaEdit.refEnd, "a tag ref keeps the rest of its line");

const notedYaml = `jobs:\n  build:\n    steps:\n      - uses: actions/checkout@${oldSha} # v4.0.0 pinned for node16\n`;
const notedResults = await checkRefs(notedYaml, mockProvider);
assert.equal(
	applyEdits(notedYaml, planUpdates(notedResults)),
	notedYaml.replace(`${oldSha} # v4.0.0`, `${newSha} # v4.2.0`),
	"a re-pin keeps the note after the version",
);
assert.equal(
	applyEdits(notedYaml, planUpdates(notedResults, { policy: { pin: "tag" } })),
	notedYaml.replace(`${oldSha} # v4.0.0`, "v4.2.0 #"),
	"an unpin keeps the note as the comment",
);

console.log("PASS: policy.pin picks tag or SHA output");

// Test 16: sortVersionTags orders by semver precedence, newest first.
const sorted = sortVersionTags([
	{ version: "v1.9.0", sha: "a".repeat(40) },
	{ version: "v1.10.0", sha: "b".repeat(40) },
	{ version: "v2.0.0-rc.1", sha: "c".repeat(40) },
	{ version: "v2.0.0", sha: "d".repeat(40) },
	{ version: "v1.10.0-beta", sha: "e".repeat(40) },
]);
assert.deepEqual(
	sorted.map((tag) => tag.version),
	["v2.0.0", "v2.0.0-rc.1", "v1.10.0", "v1.10.0-beta", "v1.9.0"],
	"tags sorted newest first",
);
assert.equal(sorted[0].sha, "d".repeat(40), "each tag keeps its sha");

console.log("PASS: sortVersionTags orders newest first");

// Test 17: core's ref parser, host rule included.
assert.deepEqual(parseUses("a.b/c@v1"), { host: null, owner: "a.b", repo: "c", subpath: null, ref_str: "v1" });
assert.deepEqual(parseUses("GitHub.COM/actions/checkout/sub@v4"), {
	host: "github.com",
	owner: "actions",
	repo: "checkout",
	subpath: "sub",
	ref_str: "v4",
});
assert.equal(parseUses("./local/action"), null, "local ref is null");
assert.throws(() => parseUses("bad owner/repo@v1"), /owner/);
assert.deepEqual(parseActionPath("localhost:8080/o/r"), { host: "localhost:8080", owner: "o", repo: "r", subpath: null });
assert.deepEqual(parseActionPath("a.b/c"), { host: null, owner: "a.b", repo: "c", subpath: null });
assert.throws(() => parseActionPath("only-owner"), /owner\/repo/);
assert.equal(isHostSegment("ghe.corp"), true);
assert.equal(isHostSegment("[::1]"), true);
assert.equal(isHostSegment("-bad.host"), false);

console.log("PASS: parseUses, parseActionPath and isHostSegment follow core");

// Test 17: the fetch provider reports a short SHA ref that abbreviates a commit.
const { createGitHubProvider } = await import("../dist/provider.js");
const fullSha = "abc1234".padEnd(40, "0");
const originalFetch = globalThis.fetch;
globalThis.fetch = async (url) => {
	const path = new URL(url).pathname;
	if (path === "/repos/actions/checkout/tags") return Response.json([{ name: "v4.0.0", commit: { sha: fullSha } }]);
	if (path === "/repos/actions/checkout/commits/abc1234") return Response.json({ sha: fullSha });
	return Response.json({ message: "Not Found" }, { status: 404 });
};
try {
	const shortYaml = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@abc1234\n";
	const [shortResult] = await checkRefs(shortYaml, createGitHubProvider());
	assert.deepEqual(shortResult.status, {
		Unresolvable: { reason: "short SHA; pin to the full 40-character commit", is_environment_error: false },
	});
} finally {
	globalThis.fetch = originalFetch;
}

console.log("PASS: fetch provider reports a short SHA ref");

// Test 18: a prerelease newer than the latest stable tag is no target, and a listed non-semver tag needs no lookup.
let lookups = 0;
const prereleaseProvider = {
	...mockProvider,
	async getVersions() {
		return {
			semverTags: [
				{ version: "v4.0.0", sha: "a".repeat(40) },
				{ version: "v4.1.0-rc.1", sha: "f".repeat(40) },
				{ version: "nightly", sha: "e".repeat(40) },
			],
		};
	},
	async resolveRefToSha() {
		lookups++;
		return "a".repeat(40);
	},
};
const [stableResult] = await checkRefs(outdatedYaml, prereleaseProvider);
assert.equal(statusName(stableResult.status), "UpToDate", `got ${JSON.stringify(stableResult.status)}`);
const nightlyYaml = "jobs:\n  build:\n    steps:\n      - uses: actions/checkout@nightly\n";
const [nightlyResult] = await checkRefs(nightlyYaml, prereleaseProvider);
assert.equal(statusName(nightlyResult.status), "UpToDate", `got ${JSON.stringify(nightlyResult.status)}`);
assert.equal(lookups, 0, "listed tags need no provider lookup");

console.log("PASS: prereleases and non-semver tags follow core tag processing");

console.log("\nAll npm package integration tests PASSED");
