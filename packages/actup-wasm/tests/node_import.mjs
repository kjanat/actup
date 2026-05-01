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

const { checkRefs, initWasm, parseUsesRefs, planUpdates } = await import("../dist/index.js");

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
assert.ok(
	validStatuses.has(statusName(checkoutResult.status)),
	`checkout status valid: got ${JSON.stringify(checkoutResult.status)}`,
);

const setupNodeResult = results.find((r) => r.action_ref?.repo === "setup-node");
assert.ok(setupNodeResult, "setup-node result present");
assert.equal(setupNodeResult.action_ref.owner, "actions", "setup-node owner");
assert.ok(
	validStatuses.has(statusName(setupNodeResult.status)),
	`setup-node status valid: got ${JSON.stringify(setupNodeResult.status)}`,
);

console.log("PASS: checkRefs returned", results.length, "results");
for (const r of results) {
	console.log("  ", JSON.stringify(r.action_ref), "->", JSON.stringify(r.status));
}

// Test 4: planUpdates
const edits = planUpdates(yaml, results);
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

console.log("\nAll npm package integration tests PASSED");
