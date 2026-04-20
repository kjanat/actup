// Integration test: verify the npm package works from Node.js.
//
// Run after `bun run build`:
//   node packages/actup-wasm/tests/node_import.mjs
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

if (!Array.isArray(results)) {
	throw new Error(`Expected array, got ${typeof results}`);
}

console.log("PASS: checkRefs returned", results.length, "results");
for (const r of results) {
	console.log("  ", JSON.stringify(r.action_ref), "->", JSON.stringify(r.status));
}

// Test 4: planUpdates
const edits = planUpdates(yaml, results);
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
