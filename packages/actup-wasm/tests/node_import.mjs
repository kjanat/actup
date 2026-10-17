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

const {
	checkRefs,
	initWasm,
	isHostSegment,
	parseActionPath,
	parseConfig,
	parseUses,
	parseUsesRefs,
	planUpdates,
	repoConfigCandidates,
	sortVersionTags,
} = await import("../dist/index.js");

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
assert.deepEqual(first.scalar_range, { start: 14, end: 33 }, "scalar range of a plain value");
assert.deepEqual(locations[1].scalar_range, { start: 14, end: 37 }, "scalar range of a quoted value covers the quotes");

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
await assert.rejects(checkRefs(outdatedYaml, majorProvider, { policy: { tracking: "exact" } }), /tracking/);
assert.throws(() => planUpdates(outdatedResults, { policy: { tracking: "exact" } }), /tracking/);
await assert.rejects(checkRefs(outdatedYaml, majorProvider, { policy: { track: "nearest" } }), /nearest/);

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
assert.equal(
	escapedLoc.raw_line.slice(escapedLoc.scalar_range.start, escapedLoc.scalar_range.end),
	"\"actions/checkout@v4.0.\\x30\"",
	"scalar range covers the escaped scalar",
);
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
	for (const e of [...edits].sort((x, y) => y.refStart - x.refStart)) {
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

const taggedYaml =
	`jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4.0.0 # v4.0.0 keep\n      - uses: actions/checkout@v4.0.0 # mine\n`;
const taggedResults = await checkRefs(taggedYaml, mockProvider);
for (const pin of ["preserve", "tag"]) {
	assert.equal(
		applyEdits(taggedYaml, planUpdates(taggedResults, { policy: { pin } })),
		taggedYaml.replace("v4.0.0 # v4.0.0 keep", "v4.2.0 # v4.2.0 keep").replace("v4.0.0 # mine", "v4.2.0 # mine"),
		`pin = ${pin} refreshes a tag's version comment and leaves other comments`,
	);
}

const steppedYaml =
	`jobs:\n  build:\n    steps:\n      - uses: actions/checkout@v4.0.0 # 1st step\n      - uses: actions/checkout@${oldSha} # 1st step\n`;
const steppedResults = await checkRefs(steppedYaml, mockProvider);
assert.equal(
	applyEdits(steppedYaml, planUpdates(steppedResults, { policy: { pin: "sha" } })),
	steppedYaml.replace("v4.0.0 # 1st step", `${newSha} # 1st step`).replace(`${oldSha} # 1st step`, `${newSha} # 1st step`),
	"a SHA target keeps a comment that names no version",
);

console.log("PASS: policy.pin picks tag or SHA output");

// Test 16: a closing quote or flow bracket stays between the ref and the comment.
const closerYaml = `\
jobs:
  build:
    steps:
      - uses: "actions/checkout@${oldSha}" # v4.0.0
      - uses: 'actions/checkout@${oldSha}'
      - { uses: actions/checkout@${oldSha} } # v4.0.0
      - [{ uses: actions/checkout@${oldSha} }, { uses: actions/checkout@${oldSha} }]
      - [{ uses: ./local }, { uses: actions/checkout@${oldSha} }]
      - { uses: actions/checkout@${oldSha}, with: { x: 1 } }
      - { uses: actions/checkout@${oldSha}, with: { x: 1 } } # v4.0.0
      - { uses: actions/checkout@${oldSha}, with: { msg: it's } } # v4.0.0
      - { uses: actions/checkout@${oldSha}, with: { a: it's, b: 'x # v9' } }
      - { uses: actions/checkout@${oldSha}, with: { b: 'it''s # v9' } }
      - { uses: actions/checkout@${oldSha}, with: { b: 'it''s # x' } } # v4.0.0
`;
const closerResults = await checkRefs(closerYaml, mockProvider);
const closerEdits = planUpdates(closerResults);
assert.equal(
	applyEdits(closerYaml, closerEdits),
	`jobs:\n  build:\n    steps:\n      - uses: "actions/checkout@${newSha}" # v4.2.0\n      - uses: 'actions/checkout@${newSha}' # v4.2.0\n      - { uses: actions/checkout@${newSha} } # v4.2.0\n      - [{ uses: actions/checkout@${newSha} }, { uses: actions/checkout@${newSha} }]\n      - [{ uses: ./local }, { uses: actions/checkout@${newSha} }]\n      - { uses: actions/checkout@${newSha}, with: { x: 1 } }\n      - { uses: actions/checkout@${newSha}, with: { x: 1 } } # v4.2.0\n      - { uses: actions/checkout@${newSha}, with: { msg: it's } } # v4.2.0\n      - { uses: actions/checkout@${newSha}, with: { a: it's, b: 'x # v9' } }\n      - { uses: actions/checkout@${newSha}, with: { b: 'it''s # v9' } }\n      - { uses: actions/checkout@${newSha}, with: { b: 'it''s # x' } } # v4.2.0\n`,
	"SHA pins write the closer before the comment, and a line with two values or other keys gets none",
);
assert.equal(
	applyEdits(closerYaml, planUpdates(closerResults, { policy: { pin: "tag" } })),
	`jobs:\n  build:\n    steps:\n      - uses: "actions/checkout@v4.2.0"\n      - uses: 'actions/checkout@v4.2.0'\n      - { uses: actions/checkout@v4.2.0 }\n      - [{ uses: actions/checkout@v4.2.0 }, { uses: actions/checkout@v4.2.0 }]\n      - [{ uses: ./local }, { uses: actions/checkout@v4.2.0 }]\n      - { uses: actions/checkout@v4.2.0, with: { x: 1 } }\n      - { uses: actions/checkout@v4.2.0, with: { x: 1 } }\n      - { uses: actions/checkout@v4.2.0, with: { msg: it's } }\n      - { uses: actions/checkout@v4.2.0, with: { a: it's, b: 'x # v9' } }\n      - { uses: actions/checkout@v4.2.0, with: { b: 'it''s # v9' } }\n      - { uses: actions/checkout@v4.2.0, with: { b: 'it''s # x' } }\n`,
	"unpinned tags keep the closer",
);

console.log("PASS: closers stay before the comment");

// Test 17: a pin-branch result pins the ref to its commit with a `# <ref>` comment.
const branchSha = "c".repeat(40);
const branchYaml = `\
jobs:
  build:
    steps:
      - uses: actions/checkout@main
      - uses: "actions/checkout@nightly" # keep me
      - { uses: actions/checkout@main } # v1.0.0
`;
const branchResults = (await checkRefs(branchYaml, mockProvider)).map((r) => ({
	...r,
	status: { PinBranch: { ref_name: r.action_ref.ref_str, sha: branchSha } },
}));
assert.equal(
	applyEdits(branchYaml, planUpdates(branchResults)),
	`jobs:\n  build:\n    steps:\n      - uses: actions/checkout@${branchSha} # main\n      - uses: "actions/checkout@${branchSha}" # keep me\n      - { uses: actions/checkout@${branchSha} } # main\n`,
	"pin-branch results pin to the commit",
);

console.log("PASS: pin-branch results get an edit");

// Test 18: sortVersionTags orders by semver precedence, newest first.
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

// Test 19: core's ref parser, host rule included.
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

// Test 20: the fetch provider reports a short SHA ref that abbreviates a commit.
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

// Test 21: a prerelease newer than the latest stable tag is no target, and a listed non-semver tag needs no lookup.
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

const rejecting = (kind, extra = {}) => ({
	...mockProvider,
	async getVersions() {
		throw Object.assign(new Error(`${kind} failure`), { kind, ...extra });
	},
});
const unresolvable = async (provider, value = "o/r@v1") => {
	const [checked] = await checkRefs(`steps:\n  - uses: ${value}\n`, provider);
	assert.equal(statusName(checked.status), "Unresolvable", `${value}: ${JSON.stringify(checked.status)}`);
	return checked.status.Unresolvable;
};
assert.deepEqual(await unresolvable(rejecting("notFound")), {
	reason: "action not found: github.com/o/r",
	is_environment_error: false,
});
assert.deepEqual(await unresolvable(rejecting("rateLimited", { retryAfter: 30 })), {
	reason: "rate limited by github, retry after 30s",
	is_environment_error: true,
});
assert.deepEqual(await unresolvable(rejecting("auth")), {
	reason: "authentication failed for github: auth failure",
	is_environment_error: true,
});
assert.deepEqual(await unresolvable(rejecting("forbidden")), {
	reason: "permission denied by github: forbidden failure",
	is_environment_error: true,
});
for (const provider of [rejecting("network"), rejecting(undefined), { ...mockProvider, getVersions: () => Promise.reject("offline") }]) {
	const status = await unresolvable(provider);
	assert.equal(status.is_environment_error, true, JSON.stringify(status));
	assert.match(status.reason, /^network error during js provider call: /);
}
const missingRef = {
	...mockProvider,
	async resolveRefToSha() {
		throw Object.assign(new Error("no such ref"), { kind: "notFound" });
	},
};
assert.deepEqual(await unresolvable(missingRef, "actions/checkout@nope"), {
	reason: "ref 'nope' not found",
	is_environment_error: false,
});

console.log("PASS: provider rejection kinds pick the error class");

const realFetch = globalThis.fetch;
const githubWith = async (respond) => {
	globalThis.fetch = async (url) => respond(String(url));
	try {
		return await unresolvable(createGitHubProvider(), "o/r@nope");
	} finally {
		globalThis.fetch = realFetch;
	}
};
const reply = (status, headers = {}) => new Response(JSON.stringify({ message: "x" }), { status, headers });
const tagList = new Response(JSON.stringify([{ name: "v1.0.0", commit: { sha: "a".repeat(40) } }]));
assert.deepEqual(await githubWith(() => reply(404)), {
	reason: "action not found: github.com/o/r",
	is_environment_error: false,
});
assert.deepEqual(
	await githubWith((url) => (url.endsWith("/tags?per_page=100") ? tagList.clone() : reply(404))),
	{ reason: "ref 'nope' not found", is_environment_error: false },
);
assert.deepEqual(await githubWith(() => reply(401)), {
	reason: "authentication failed for github: GitHub API authentication failed (401)",
	is_environment_error: true,
});
assert.deepEqual(await githubWith(() => reply(403)), {
	reason: "permission denied by github: GitHub API forbidden (403)",
	is_environment_error: true,
});
assert.deepEqual(await githubWith(() => reply(429, { "retry-after": "60" })), {
	reason: "rate limited by github, retry after 60s",
	is_environment_error: true,
});
assert.deepEqual(await githubWith(() => reply(403, { "x-ratelimit-remaining": "0" })), {
	reason: "rate limited by github",
	is_environment_error: true,
});
assert.match((await githubWith(() => reply(502))).reason, /^network error during js provider call: GitHub API error: 502/);

console.log("PASS: GitHub provider statuses map to error classes");

const twinTags = {
	...mockProvider,
	async getVersions() {
		return {
			semverTags: [
				{ version: "v1", sha: "e".repeat(40) },
				{ version: "v1.0.4", sha: "f".repeat(40) },
				{ version: "v1.0.5", sha: "e".repeat(40) },
			],
		};
	},
};
const twinYaml = "steps:\n  - uses: o/r@v1\n";
const [kept] = await checkRefs(twinYaml, twinTags);
assert.equal(kept.status, "UpToDate", "track keep leaves v1 alone");
assert.equal(kept.ref_kind, "tag", "v1 names a tag");
assert.equal(kept.same_commit, null, "no same-commit target without an update");
const [exact] = await checkRefs(twinYaml, twinTags, { policy: { track: "exact" } });
assert.deepEqual(exact.status, { Outdated: { current: "v1", target: { version: "v1.0.5", sha: "e".repeat(40) } } });
assert.equal(exact.ref_kind, "tag", "v1 names a tag");
assert.deepEqual(exact.same_commit, { version: "v1.0.5", sha: "e".repeat(40) }, "target sits on the v1 commit");
const [behind] = await checkRefs("steps:\n  - uses: o/r@v1.0.4\n", twinTags, { policy: { track: "exact" } });
assert.equal(behind.same_commit, null, "a real bump has no same-commit target");
assert.equal(planUpdates([exact], { policy: { track: "exact" } })[0].newText, "v1.0.5", "planUpdates reads the new fields");

console.log("PASS: policy.track reaches core and same-commit targets serialize");

const candidates = repoConfigCandidates();
assert.equal(candidates[0], "actup.toml", "actup.toml comes first");
assert.ok(candidates.includes(".actup.jsonc"), "dot-files at the root count");
assert.ok(candidates.includes(".config/actup.yml"), ".config holds candidates");
assert.equal(candidates.length, 20, "every name, directory and extension");

const minorFile = parseConfig(".config/actup.toml", "[policy]\nbump = \"minor\"\n");
const [minorFromFile] = await checkRefs(outdatedYaml, majorProvider, minorFile);
assert.equal(minorFromFile.status, "UpToDate", "policy.bump from the file reaches checkRefs");
const minorYaml = parseConfig("actup.yaml", "policy:\n  bump: minor\n");
assert.equal((await checkRefs(outdatedYaml, majorProvider, minorYaml))[0].status, "UpToDate", "YAML parses");

const overridden = parseConfig(
	"actup.toml",
	"[[overrides]]\npattern = \"actions/*\"\npolicy = { pin = \"sha\" }\n",
);
const overriddenResults = await checkRefs(outdatedYaml, majorProvider, overridden);
assert.equal(overriddenResults[0].status.Outdated.target.version, "v5.0.0", "the override keeps the default bump");
const [overrideEdit] = planUpdates(overriddenResults, overridden);
assert.equal(overrideEdit.newRef, "e".repeat(40), "the override's pin style reaches planUpdates");
assert.equal(overrideEdit.newText, `${"e".repeat(40)} # v5.0.0`, "a SHA pin carries its version comment");
assert.equal(planUpdates(overriddenResults)[0].newRef, "v5.0.0", "without the config the tag stays");

const ignoring = parseConfig("actup.json", "{ \"scan\": { \"ignore\": [\"actions/checkout\"] } }");
assert.equal((await checkRefs(outdatedYaml, majorProvider, ignoring))[0].status, "Ignored", "scan.ignore reaches checkRefs");

assert.throws(() => parseConfig("actup.toml", "[policy]\ntracking = 1\n"), /actup\.toml.*tracking/s);
assert.throws(() => parseConfig("actup.toml", "update_policy = \"patch\"\n"), /update_policy/);

console.log("PASS: parseConfig parses repo config files for checkRefs and planUpdates");

// Test 22: refs that are not git ref names reach no provider.
for (const uses of ["o/r@../../x/y", "o/r@./x", "o/r@a/.b"]) {
	assert.throws(() => parseUses(uses), /ref/, uses);
}
const calls = [];
const recordingProvider = {
	...mockProvider,
	async getVersions(host, owner, repo) {
		calls.push(`getVersions ${owner}/${repo}`);
		return { semverTags: [] };
	},
	async resolveRefToSha(host, owner, repo, ref) {
		calls.push(`resolveRefToSha ${owner}/${repo}@${ref}`);
		return "a".repeat(40);
	},
};
const dottedYaml =
	`jobs:\n  build:\n    steps:\n      - uses: o/r@../../x/y\n      - uses: o/r@./x\n      - uses: o/r@a/.b\n      - uses: o/r@a~1\n      - uses: o/r\n      - uses: o/r@\n      - uses: ./local\n        with:\n          uses: lint\n`;
const invalidResults = await checkRefs(dottedYaml, recordingProvider);
assert.deepEqual(
	invalidResults.map((r) => [r.location.line, r.action_ref.owner, r.action_ref.repo, r.action_ref.ref_str]),
	[
		[4, "o", "r", "../../x/y"],
		[5, "o", "r", "./x"],
		[6, "o", "r", "a/.b"],
		[7, "o", "r", "a~1"],
		[8, "o", "r", ""],
		[9, "o", "r", ""],
	],
	"one result for each invalid ref",
);
for (const r of invalidResults) {
	assert.equal(statusName(r.status), "Unresolvable", JSON.stringify(r.status));
	assert.equal(r.status.Unresolvable.is_environment_error, false, "an invalid ref is a user error");
	assert.match(r.status.Unresolvable.reason, /^invalid action reference/);
}
assert.deepEqual(calls, [], "no provider call for invalid refs");

console.log("PASS: invalid git refs are unresolvable and reach no provider");

console.log("\nAll npm package integration tests PASSED");
