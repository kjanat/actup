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

const setupNode = results[1];
const setupNodeTarget = setupNode?.status?.Outdated?.target?.version;
if (setupNodeTarget !== "v4.0.0") {
	throw new Error(
		`Expected setup-node to target v4.0.0, got ${JSON.stringify(setupNode?.status)}`,
	);
}
console.log("PASS: setup-node targets v4.0.0");

// Test planUpdates
const edits = planUpdates(JSON.stringify(results));

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

const pinnedYaml = `\
jobs:
  build:
    steps:
      - uses: actions/setup-node@v3.0.0
`;
const [defaultSetupNode] = await checkRefs(pinnedYaml, mockProvider);
const defaultTarget = defaultSetupNode?.status?.Outdated?.target?.version;
if (defaultTarget !== "v4.0.0") {
	throw new Error(
		`Expected default policy to target setup-node v4.0.0, got ${JSON.stringify(defaultSetupNode?.status)}`,
	);
}
const [minorSetupNode] = await checkRefs(pinnedYaml, mockProvider, { policy: { bump: "minor" } });
const minorTarget = minorSetupNode?.status?.Outdated?.target?.version;
if (minorTarget !== "v3.1.0") {
	throw new Error(
		`Expected policy.bump = minor to target setup-node v3.1.0, got ${JSON.stringify(minorSetupNode?.status)}`,
	);
}
console.log("PASS: policy.bump = minor targets setup-node v3.1.0");

// Test empty YAML
const emptyResults = await checkRefs("", mockProvider);
if (!Array.isArray(emptyResults) || emptyResults.length !== 0) {
	throw new Error(
		`Expected empty array for empty YAML, got ${JSON.stringify(emptyResults)}`,
	);
}
console.log("PASS: empty YAML returns empty results");

// Test planUpdates with no outdated results (pass up-to-date)
const noEdits = planUpdates("[]");
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
	const fullEdits = planUpdates(JSON.stringify(results));
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

	// Negative path: provider throws → each affected ref surfaces as
	// Unresolvable status with the provider's error message. checkRefs
	// itself does not reject — errors are converted per-ref so one bad
	// repo doesn't break the whole batch.
	const failingProvider = {
		...mockProvider,
		async getVersions() {
			throw new Error("synthetic provider failure");
		},
	};
	const failingResults = await checkRefs(yaml, failingProvider);
	if (failingResults.length === 0) {
		throw new Error("Negative path: expected at least one result from failing provider");
	}
	const firstStatus = failingResults[0].status;
	const unresolvable = typeof firstStatus === "object" && firstStatus !== null
		? firstStatus.Unresolvable
		: undefined;
	if (unresolvable === undefined) {
		throw new Error(`Negative path: expected Unresolvable status, got ${JSON.stringify(firstStatus)}`);
	}
	if (!unresolvable.reason.includes("synthetic provider failure")) {
		throw new Error(`Negative path: reason did not propagate provider message: ${unresolvable.reason}`);
	}
	console.log("PASS: negative path — provider errors surface as Unresolvable results");
}

console.log("\nAll checkRefs + planUpdates tests PASSED");
