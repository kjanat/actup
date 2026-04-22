// Integration test: verify checkRefs + planUpdates with a mock JS provider
//
// Run after `wasm-pack build crates/wasm --target nodejs --out-dir ../../target/wasm-pkg`:
//   node crates/wasm/tests/node_check.mjs

import { checkRefs, parseUsesRefs, planUpdates } from "../../../target/wasm-pkg/actup_wasm.js";

// Mock provider that returns controlled version data
const mockProvider = {
	async getVersions(host, owner, repo) {
		if (owner === "actions" && repo === "checkout") {
			return {
				semverTags: [
					{ version: "v3.0.0", sha: "a".repeat(40) },
					{ version: "v4.0.0", sha: "b".repeat(40) },
					{ version: "v4.1.0", sha: "c".repeat(40) },
					{ version: "v4.2.0", sha: "d".repeat(40) },
				],
			};
		}
		if (owner === "actions" && repo === "setup-node") {
			return {
				semverTags: [
					{ version: "v3.0.0", sha: "e".repeat(40) },
					{ version: "v3.1.0", sha: "f".repeat(40) },
					{ version: "v4.0.0", sha: "1".repeat(40) },
				],
			};
		}
		throw new Error(`Unknown repo: ${owner}/${repo}`);
	},

	async resolveRefToSha(host, owner, repo, ref) {
		return "a".repeat(40);
	},

	async getDefaultBranch(host, owner, repo) {
		return "main";
	},

	async getReadme(host, owner, repo, ref) {
		return null;
	},

	async getActionYml(host, owner, repo, ref) {
		return { raw: "", name: null, description: null, author: null, inputs: {}, outputs: {}, runs: null };
	},
};

const yaml = `\
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v3
      - uses: actions/setup-node@v3
      - uses: './local/action'
`;

// Test checkRefs
const results = await checkRefs(yaml, mockProvider);

if (!Array.isArray(results)) {
	throw new Error(`Expected array, got ${typeof results}`);
}

// Should have 2 results (local ref is skipped)
if (results.length !== 2) {
	throw new Error(
		`Expected 2 results, got ${results.length}: ${JSON.stringify(results, null, 2)}`,
	);
}

// First result: actions/checkout@v3 should be outdated (v4.2.0 available)
const checkout = results[0];
if (checkout.status !== "Outdated") {
	// serde enum uses variant name
	const statusKey = Object.keys(checkout.status)[0] || checkout.status;
	if (statusKey !== "Outdated") {
		throw new Error(
			`Expected checkout status Outdated, got ${JSON.stringify(checkout.status)}`,
		);
	}
}
console.log("PASS: checkout is outdated:", JSON.stringify(checkout.status));

// Second result: actions/setup-node@v3 — could be outdated (v4.0.0 available)
// depending on default policy (patch = same major, so v3.1.0 is target)
const setupNode = results[1];
console.log(
	"PASS: setup-node status:",
	JSON.stringify(setupNode.status),
);

// Test planUpdates
const resultsJson = JSON.stringify(results);
const edits = planUpdates(yaml, resultsJson);

if (!Array.isArray(edits)) {
	throw new Error(`Expected edits array, got ${typeof edits}`);
}

console.log(
	"PASS: planUpdates returned",
	edits.length,
	"edits:",
	JSON.stringify(edits, null, 2),
);

// Verify edit structure
for (const edit of edits) {
	if (typeof edit.line !== "number") {
		throw new Error(`Expected edit.line to be number, got ${typeof edit.line}`);
	}
	if (typeof edit.refStart !== "number") {
		throw new Error(
			`Expected edit.refStart to be number, got ${typeof edit.refStart}`,
		);
	}
	if (typeof edit.refEnd !== "number") {
		throw new Error(
			`Expected edit.refEnd to be number, got ${typeof edit.refEnd}`,
		);
	}
	if (typeof edit.original !== "string") {
		throw new Error(
			`Expected edit.original to be string, got ${typeof edit.original}`,
		);
	}
	if (typeof edit.newRef !== "string") {
		throw new Error(
			`Expected edit.newRef to be string, got ${typeof edit.newRef}`,
		);
	}
}

// Test with custom config (major policy — allows cross-major updates)
const majorConfig = JSON.stringify({ update_policy: "major" });
const majorResults = await checkRefs(yaml, mockProvider, majorConfig);

console.log("PASS: checkRefs with major policy returned", majorResults.length, "results");
for (const r of majorResults) {
	console.log(
		"  ",
		JSON.stringify(r.action_ref),
		"->",
		JSON.stringify(r.status),
	);
}

// Test empty YAML
const emptyResults = await checkRefs("", mockProvider);
if (!Array.isArray(emptyResults) || emptyResults.length !== 0) {
	throw new Error(
		`Expected empty array for empty YAML, got ${JSON.stringify(emptyResults)}`,
	);
}
console.log("PASS: empty YAML returns empty results");

// Test planUpdates with no outdated results (pass up-to-date)
const noEdits = planUpdates(yaml, "[]");
if (!Array.isArray(noEdits) || noEdits.length !== 0) {
	throw new Error(
		`Expected no edits for empty results, got ${JSON.stringify(noEdits)}`,
	);
}
console.log("PASS: planUpdates with empty results returns no edits");

// Roundtrip test: apply edits to the YAML and re-parse to confirm
// (a) the new @ref appears at the expected location, and
// (b) the resulting YAML still parses cleanly via parseUsesRefs.
{
	const fullEdits = planUpdates(yaml, JSON.stringify(results));
	if (fullEdits.length === 0) {
		throw new Error("Roundtrip: expected at least one edit for the outdated checkout ref");
	}

	// Apply edits in reverse line order so earlier offsets stay valid.
	const sorted = [...fullEdits].sort((a, b) => b.line - a.line);
	const lines = yaml.split("\n");
	for (const edit of sorted) {
		const idx = edit.line - 1;
		const line = lines[idx];
		if (line === undefined) {
			throw new Error(`Roundtrip: edit references missing line ${edit.line}`);
		}
		lines[idx] = line.slice(0, edit.refStart) + edit.newRef + line.slice(edit.refEnd);
	}
	const updated = lines.join("\n");

	// Re-parse and verify the new refs appear where expected.
	const parsedAgain = parseUsesRefs(updated);
	if (!Array.isArray(parsedAgain)) {
		throw new Error(`Roundtrip: parseUsesRefs did not return an array after edit`);
	}

	for (const edit of fullEdits) {
		const match = parsedAgain.find((loc) => loc.line === edit.line);
		if (match === undefined) {
			throw new Error(`Roundtrip: no parsed uses-ref on line ${edit.line} after edit`);
		}
		if (!match.value.endsWith("@" + edit.newRef)) {
			throw new Error(
				`Roundtrip: line ${edit.line} value "${match.value}" does not end with @${edit.newRef}`,
			);
		}
	}
	console.log("PASS: roundtrip — edits apply cleanly and re-parse to expected refs");

	// Negative path: provider that throws should surface as a rejected promise.
	const failingProvider = {
		...mockProvider,
		async getVersions() {
			throw new Error("synthetic provider failure");
		},
	};
	let surfaced = false;
	try {
		await checkRefs(yaml, failingProvider);
	} catch (err) {
		surfaced = true;
		if (!String(err).includes("synthetic provider failure")) {
			throw new Error(`Negative path: error did not propagate provider message: ${err}`);
		}
	}
	if (!surfaced) {
		throw new Error("Negative path: expected checkRefs to reject when provider throws");
	}
	console.log("PASS: negative path — provider errors surface as rejected promises");
}

console.log("\nAll checkRefs + planUpdates tests PASSED");
