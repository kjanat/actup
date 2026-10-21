import * as wasm from 'actup-wasm';
import { describe, expect, mock, test } from 'bun:test';
import * as vscode from 'vscode';
import { ActupCodeActionProvider } from '../codeActions.ts';
import {
	checkResultsByUri,
	clearDiagnosticData,
	computeDiagnostics,
	createDiagnosticCollection,
	diagnosticDataByUri,
	diagnosticKey,
} from '../diagnostics.ts';
import { codeActionContext, editsOf, FakeDocument, fakeProvider, gate, gatedProvider, rangeOf } from './fixtures.ts';

const provider = fakeProvider({
	'acme/workflows': [
		{ version: 'v1.0.0', sha: 'a'.repeat(40) },
		{ version: 'v1.2.0', sha: 'b'.repeat(40) },
	],
});

const workflow = [
	'jobs:',
	'  ci:',
	'    uses: github.com/acme/workflows/.github/workflows/ci.yml@v1.0.0',
	'  lint:',
	'    uses: github.com/acme/workflows/.github/workflows/lint.yml@v1.0.0',
	'  gone:',
	'    uses: github.com/acme/missing/.github/workflows/ci.yml@v1.0.0',
	'  escaped:',
	'    uses: "github.com/acme/missing/.github/workflows/ci.yml@v1.0.\\x30"',
	'',
].join('\n');

async function diagnose(name: string, text: string): Promise<{ document: FakeDocument; diagnostics: vscode.Diagnostic[] }> {
	const document = new FakeDocument(name, text);
	await computeDiagnostics(document.uri, text, provider, createDiagnosticCollection());
	return { document, diagnostics: vscode.languages.getDiagnostics(document.uri) };
}

describe('computeDiagnostics', () => {
	test('covers the ref of an outdated value and stores its edit data', async () => {
		const { document, diagnostics } = await diagnose('ranges.yml', workflow);
		const [ci, lint] = diagnostics;

		expect(ci?.range).toEqual(rangeOf(document, 2, 'v1.0.0'));
		expect(ci?.severity).toBe(vscode.DiagnosticSeverity.Warning);
		expect(ci?.code).toBe('outdated');
		expect(lint?.range).toEqual(rangeOf(document, 4, 'v1.0.0'));
		expect([...(diagnosticDataByUri.get(document.uri.toString())?.values() ?? [])]).toEqual([
			{ kind: 'outdated', targetRef: 'v1.2.0', range: rangeOf(document, 2, 'v1.0.0'), newText: 'v1.2.0' },
			{ kind: 'outdated', targetRef: 'v1.2.0', range: rangeOf(document, 4, 'v1.0.0'), newText: 'v1.2.0' },
		]);
	});

	test('covers the ref of an unresolvable value', async () => {
		const { document, diagnostics } = await diagnose('unresolvable.yml', workflow);
		const gone = diagnostics[2];

		expect(gone?.range).toEqual(rangeOf(document, 6, 'v1.0.0'));
		expect(gone?.severity).toBe(vscode.DiagnosticSeverity.Information);
		expect(gone?.code).toBe('unresolvable');
	});

	test('marks a repository the provider reports missing as an error', async () => {
		const missing: wasm.ActupProvider = {
			...provider,
			getVersions: () => Promise.reject(Object.assign(new Error('no repository'), { kind: 'notFound' })),
		};
		const document = new FakeDocument('missing-repo.yml', 'jobs:\n  ci:\n    uses: acme/gone/.github/workflows/ci.yml@v1.0.0\n');
		await computeDiagnostics(document.uri, document.getText(), missing, createDiagnosticCollection());
		const [gone] = vscode.languages.getDiagnostics(document.uri);

		expect(gone?.severity).toBe(vscode.DiagnosticSeverity.Error);
		expect(gone?.code).toBe('unresolvable');
		expect(gone?.message).toBe('acme/gone/.github/workflows/ci.yml@v1.0.0: action not found: github.com/acme/gone');
	});

	test('covers the whole line when the line does not hold the value verbatim', async () => {
		const { document, diagnostics } = await diagnose('whole-line.yml', workflow);
		const escaped = diagnostics[3];

		expect(diagnostics).toHaveLength(4);
		expect(escaped?.range).toEqual(document.lineAt(8).range);
		expect(escaped?.code).toBe('unresolvable');
	});
});

describe('ActupCodeActionProvider', () => {
	test('offers edits only for diagnostics with edit data', async () => {
		const { document, diagnostics } = await diagnose('actions.yml', workflow);
		const actions = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(0).range,
			codeActionContext(diagnostics),
		);

		expect(actions.map(action => action.title)).toEqual([
			'Update to v1.2.0',
			'Update to v1.2.0',
			'Fix all outdated actions (2 refs)',
		]);
		const [ciFix, lintFix, fixAll] = actions;
		expect(editsOf(ciFix)).toEqual([{ range: rangeOf(document, 2, 'v1.0.0'), newText: 'v1.2.0' }]);
		expect(editsOf(lintFix)).toEqual([{ range: rangeOf(document, 4, 'v1.0.0'), newText: 'v1.2.0' }]);
		expect(editsOf(fixAll)).toEqual([
			{ range: rangeOf(document, 2, 'v1.0.0'), newText: 'v1.2.0' },
			{ range: rangeOf(document, 4, 'v1.0.0'), newText: 'v1.2.0' },
		]);
		expect(fixAll?.diagnostics?.map(diagnostic => diagnostic.code)).toEqual(['outdated', 'outdated']);
	});
});

describe('an outdated value the line holds escaped', () => {
	const escapedWorkflow = [
		'jobs:',
		'  ci:',
		'    uses: github.com/acme/workflows/.github/workflows/ci.yml@v1.0.0',
		'  lint:',
		'    uses: github.com/acme/workflows/.github/workflows/lint.yml@v1.0.0',
		'  escaped:',
		'    uses: "github.com/acme/workflows/.github/workflows/ci.yml@v1.0.\\x30"',
		'',
	].join('\n');

	test('gets a whole-line diagnostic without edit data', async () => {
		const { document, diagnostics } = await diagnose('escaped-outdated.yml', escapedWorkflow);
		const escaped = diagnostics[2];
		if (escaped === undefined) {
			throw new Error('no diagnostic for the escaped value');
		}
		const dataMap = diagnosticDataByUri.get(document.uri.toString());

		expect(diagnostics).toHaveLength(3);
		expect(escaped.range).toEqual(document.lineAt(6).range);
		expect(escaped.code).toBe('outdated');
		expect(dataMap?.get(diagnosticKey(escaped))).toBeUndefined();
		expect(dataMap?.size).toBe(2);
	});

	test('is skipped by the quick fix and by fix-all', async () => {
		const { document, diagnostics } = await diagnose('escaped-actions.yml', escapedWorkflow);
		const escaped = diagnostics.filter(diagnostic => diagnostic.range.start.line === 6);
		const actions = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(6).range,
			codeActionContext(escaped),
		);

		expect(escaped).toHaveLength(1);
		expect(actions.map(action => action.title)).toEqual(['Fix all outdated actions (2 refs)']);
		expect(editsOf(actions[0])).toEqual([
			{ range: rangeOf(document, 2, 'v1.0.0'), newText: 'v1.2.0' },
			{ range: rangeOf(document, 4, 'v1.0.0'), newText: 'v1.2.0' },
		]);
	});
});

describe('a check that finishes after a newer computation or a close', () => {
	function reusable(ref: string): string {
		return `jobs:\n  ci:\n    uses: acme/workflows/.github/workflows/ci.yml@${ref}\n`;
	}

	function storedValues(uri: vscode.Uri): string[] | undefined {
		return checkResultsByUri.get(uri.toString())?.map(result => result.location.value);
	}

	test('leaves the newer results in place', async () => {
		const uri = vscode.Uri.parse('file:///stale-success.yml');
		const collection = createDiagnosticCollection();
		const held = gate();
		const older = computeDiagnostics(uri, reusable('v1.0.0'), gatedProvider(provider, held), collection);
		await computeDiagnostics(uri, reusable('v1.2.0'), provider, collection);
		held.open();
		await older;

		expect(collection.get(uri)).toEqual([]);
		expect(storedValues(uri)).toEqual(['acme/workflows/.github/workflows/ci.yml@v1.2.0']);
		expect(diagnosticDataByUri.get(uri.toString())?.size).toBe(0);
	});

	test('leaves the newer results in place when the older check fails', async () => {
		const uri = vscode.Uri.parse('file:///stale-failure.yml');
		const collection = createDiagnosticCollection();
		const held = gate();
		const real = { ...wasm };
		let calls = 0;
		await mock.module('actup-wasm', () => ({
			...real,
			checkRefs: (text: string, p: wasm.ActupProvider) =>
				calls++ === 0
					? held.opened.then(() => Promise.reject(new Error('offline')))
					: real.checkRefs(text, p),
		}));
		try {
			const older = computeDiagnostics(uri, reusable('v1.0.0'), provider, collection);
			await computeDiagnostics(uri, reusable('v1.0.0'), provider, collection);
			held.open();
			await older;
		} finally {
			await mock.module('actup-wasm', () => real);
		}

		expect(collection.get(uri)?.map(diagnostic => diagnostic.code)).toEqual(['outdated']);
		expect(storedValues(uri)).toEqual(['acme/workflows/.github/workflows/ci.yml@v1.0.0']);
		expect(diagnosticDataByUri.get(uri.toString())?.size).toBe(1);
	});

	test('writes nothing once the document has closed', async () => {
		const uri = vscode.Uri.parse('file:///stale-closed.yml');
		const collection = createDiagnosticCollection();
		const held = gate();
		const older = computeDiagnostics(uri, reusable('v1.0.0'), gatedProvider(provider, held), collection);
		clearDiagnosticData(uri);
		held.open();
		await older;

		expect(collection.get(uri)).toBeUndefined();
		expect(checkResultsByUri.has(uri.toString())).toBe(false);
		expect(diagnosticDataByUri.has(uri.toString())).toBe(false);
	});
});

describe('an outdated SHA pin with a version comment', () => {
	const pinned = [
		'jobs:',
		'  ci:',
		`    uses: acme/workflows/.github/workflows/ci.yml@${'a'.repeat(40)} # v1.0.0`,
		'  lint:',
		`    uses: acme/workflows/.github/workflows/lint.yml@${'a'.repeat(40)} # v1.0.0`,
		'',
	].join('\n');
	const repinned = `${'b'.repeat(40)} # v1.2.0`;

	function toLineEnd(document: FakeDocument, line: number): vscode.Range {
		const start = rangeOf(document, line, 'a'.repeat(40)).start;
		return new vscode.Range(line, start.character, line, document.lineAt(line).text.length);
	}

	test('gets a quick fix that re-pins the SHA and its comment', async () => {
		const { document, diagnostics } = await diagnose('sha-quick-fix.yml', pinned);
		const ci = diagnostics.filter(diagnostic => diagnostic.range.start.line === 2);
		const [quickFix] = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(2).range,
			codeActionContext(ci),
		);

		expect(ci.map(diagnostic => diagnostic.range)).toEqual([rangeOf(document, 2, 'a'.repeat(40))]);
		expect(quickFix?.title).toBe('Update to v1.2.0');
		expect(editsOf(quickFix)).toEqual([{ range: toLineEnd(document, 2), newText: repinned }]);
	});

	test('gets a fix-all that re-pins every SHA and its comment', async () => {
		const { document, diagnostics } = await diagnose('sha-fix-all.yml', pinned);
		const actions = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(0).range,
			codeActionContext(diagnostics),
		);

		expect(actions.map(action => action.title)).toEqual([
			'Update to v1.2.0',
			'Update to v1.2.0',
			'Fix all outdated actions (2 refs)',
		]);
		expect(editsOf(actions[2])).toEqual([
			{ range: toLineEnd(document, 2), newText: repinned },
			{ range: toLineEnd(document, 4), newText: repinned },
		]);
	});
});

describe('a target on the commit the ref already names', () => {
	const twins = fakeProvider({
		'o/r': [
			{ version: 'v1', sha: 'e'.repeat(40) },
			{ version: 'v1.0.4', sha: 'f'.repeat(40) },
			{ version: 'v1.0.5', sha: 'e'.repeat(40) },
		],
	});

	async function withTrack<T>(track: 'major' | 'exact', run: () => Promise<T>): Promise<T> {
		const real = { ...wasm };
		await mock.module('actup-wasm', () => ({
			...real,
			checkRefs: (text: string, p: wasm.ActupProvider) => real.checkRefs(text, p, { policy: { track } }),
		}));
		try {
			return await run();
		} finally {
			await mock.module('actup-wasm', () => real);
		}
	}

	test('gets an information diagnostic like the LSP, with its pin fix', async () => {
		const document = new FakeDocument('same-commit.yml', 'jobs:\n  ci:\n    steps:\n      - uses: o/r@v1\n');
		await withTrack('exact', () => computeDiagnostics(document.uri, document.getText(), twins, createDiagnosticCollection()));
		const [same] = vscode.languages.getDiagnostics(document.uri);
		if (same === undefined) {
			throw new Error('no diagnostic for the same-commit ref');
		}

		expect(same.severity).toBe(vscode.DiagnosticSeverity.Information);
		expect(same.code).toBe('same-commit');
		expect(same.message).toBe('o/r@v1: pin available, same commit: tag v1 = v1.0.5 (eeeeeee)');
		expect(same.range).toEqual(rangeOf(document, 3, 'v1'));
		expect(diagnosticDataByUri.get(document.uri.toString())?.get(diagnosticKey(same))?.newText).toBe('v1.0.5');
	});

	test('offers no pin for a less specific target, like the LSP', async () => {
		const document = new FakeDocument('same-commit-alias.yml', 'jobs:\n  ci:\n    steps:\n      - uses: o/r@v1.0.5\n');
		await withTrack('major', () => computeDiagnostics(document.uri, document.getText(), twins, createDiagnosticCollection()));
		const [same] = vscode.languages.getDiagnostics(document.uri);

		expect(same?.severity).toBe(vscode.DiagnosticSeverity.Information);
		expect(same?.code).toBe('same-commit');
		expect(same?.message).toBe('o/r@v1.0.5: same commit: tag v1.0.5 = v1 (eeeeeee)');
	});

	test('keeps a real bump a warning', async () => {
		const document = new FakeDocument('real-bump.yml', 'jobs:\n  ci:\n    steps:\n      - uses: o/r@v1.0.4\n');
		await withTrack('exact', () => computeDiagnostics(document.uri, document.getText(), twins, createDiagnosticCollection()));
		const [bump] = vscode.languages.getDiagnostics(document.uri);

		expect(bump?.severity).toBe(vscode.DiagnosticSeverity.Warning);
		expect(bump?.code).toBe('outdated');
	});
});

describe('floating and pin-branch refs', () => {
	const head = 'd'.repeat(40);
	const branchProvider: wasm.ActupProvider = {
		...provider,
		getVersions: () =>
			Promise.resolve({
				semverTags: [{ version: 'v1.0.0', sha: 'a'.repeat(40) }, { version: 'v1.2.0', sha: 'b'.repeat(40) }],
				defaultBranch: { name: 'main', sha: head },
			}),
	};
	const text = [
		'jobs:',
		'  build:',
		'    steps:',
		'      - uses: actions/checkout@main',
		'      - uses: actions/cache@main',
		'      - uses: actions/setup-node@v1.0.0',
		'',
	].join('\n');

	async function diagnoseUnder(name: string, policy: string): Promise<{ document: FakeDocument; diagnostics: vscode.Diagnostic[] }> {
		const document = new FakeDocument(name, text);
		const config = wasm.parseConfig('actup.toml', policy);
		await computeDiagnostics(
			document.uri,
			text,
			branchProvider,
			createDiagnosticCollection(),
			() => Promise.resolve({ kind: 'loaded', path: 'actup.toml', config }),
		);
		return { document, diagnostics: vscode.languages.getDiagnostics(document.uri) };
	}

	test('warn about a floating branch under policy.branch = warn and offer no edit', async () => {
		const { document, diagnostics } = await diagnoseUnder('floating.yml', '[policy]\nbranch = "warn"\n');
		const [checkout, cache, setupNode] = diagnostics;

		expect(checkout?.range).toEqual(rangeOf(document, 3, 'main'));
		expect(checkout?.severity).toBe(vscode.DiagnosticSeverity.Warning);
		expect(checkout?.code).toBe('floating');
		expect(checkout?.message).toBe("actions/checkout@main: 'main' is a floating branch, pin to a tag or SHA");
		expect(cache?.code).toBe('floating');
		expect(setupNode?.code).toBe('outdated');
		const actions = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(0).range,
			codeActionContext(diagnostics),
		);
		expect(actions.map(action => action.title)).toEqual(['Update to v1.2.0']);
	});

	test('offer the pin from planUpdates under policy.branch = update, in the quick fix and fix-all', async () => {
		const { document, diagnostics } = await diagnoseUnder('pin-branch.yml', '[policy]\nbranch = "update"\n');
		const [checkout, cache, setupNode] = diagnostics;

		expect(checkout?.range).toEqual(rangeOf(document, 3, 'main'));
		expect(checkout?.severity).toBe(vscode.DiagnosticSeverity.Warning);
		expect(checkout?.code).toBe('pin-branch');
		expect(checkout?.message).toBe("actions/checkout@main: pin branch 'main' to ddddddd # main");
		expect(cache?.code).toBe('pin-branch');
		expect(setupNode?.code).toBe('outdated');

		const actions = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(0).range,
			codeActionContext(diagnostics),
		);
		expect(actions.map(action => action.title)).toEqual([
			'Pin to ddddddd # main',
			'Pin to ddddddd # main',
			'Update to v1.2.0',
			'Fix all outdated actions (3 refs)',
		]);
		const [checkoutFix, , , fixAll] = actions;
		expect(editsOf(checkoutFix)).toEqual([{ range: rangeOf(document, 3, 'main'), newText: `${head} # main` }]);
		expect(editsOf(fixAll)).toEqual([
			{ range: rangeOf(document, 3, 'main'), newText: `${head} # main` },
			{ range: rangeOf(document, 4, 'main'), newText: `${head} # main` },
			{ range: rangeOf(document, 5, 'v1.0.0'), newText: 'v1.2.0' },
		]);
	});
});

describe('SHA-pinned refs', () => {
	const known = 'b'.repeat(40);
	const unknown = '9'.repeat(40);
	const text = [
		'jobs:',
		'  build:',
		'    steps:',
		`      - uses: acme/workflows@${known}`,
		`      - uses: acme/workflows@${unknown}`,
		'',
	].join('\n');

	test('get a pinned hint naming the tag, or an unknown version, and no edit', async () => {
		const { document, diagnostics } = await diagnose('pinned.yml', text);
		const [tagged, untagged] = diagnostics;

		expect(diagnostics).toHaveLength(2);
		expect(tagged?.range).toEqual(rangeOf(document, 3, known));
		expect(tagged?.severity).toBe(vscode.DiagnosticSeverity.Hint);
		expect(tagged?.code).toBe('pinned');
		expect(tagged?.message).toBe(`acme/workflows@${known}: pinned to bbbbbbb (= v1.2.0)`);
		expect(untagged?.range).toEqual(rangeOf(document, 4, unknown));
		expect(untagged?.severity).toBe(vscode.DiagnosticSeverity.Hint);
		expect(untagged?.code).toBe('pinned');
		expect(untagged?.message).toBe(`acme/workflows@${unknown}: pinned to 9999999 (unknown version)`);
		expect(diagnosticDataByUri.get(document.uri.toString())?.size).toBe(0);
		const actions = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(0).range,
			codeActionContext(diagnostics),
		);
		expect(actions).toEqual([]);
	});
});

describe('branch refs that actup-lsp words specially', () => {
	async function diagnoseRef(
		name: string,
		ref: string,
		using: wasm.ActupProvider,
		policy: string,
	): Promise<{ document: FakeDocument; diagnostics: vscode.Diagnostic[] }> {
		const text = `jobs:\n  build:\n    steps:\n      - uses: actions/checkout@${ref}\n`;
		const document = new FakeDocument(name, text);
		const config = wasm.parseConfig('actup.toml', policy);
		await computeDiagnostics(
			document.uri,
			text,
			using,
			createDiagnosticCollection(),
			() => Promise.resolve({ kind: 'loaded', path: 'actup.toml', config }),
		);
		return { document, diagnostics: vscode.languages.getDiagnostics(document.uri) };
	}

	test('a branch on the target commit gets same-commit information and keeps its pin', async () => {
		const head = 'e'.repeat(40);
		const branchOnRelease: wasm.ActupProvider = {
			...provider,
			getVersions: () =>
				Promise.resolve({
					semverTags: [{ version: 'v1.0.4', sha: 'c'.repeat(40) }, { version: 'v1.0.5', sha: head }],
					defaultBranch: { name: 'v1', sha: head },
				}),
		};
		const { document, diagnostics } = await diagnoseRef(
			'same-commit-branch.yml',
			'v1',
			branchOnRelease,
			'[policy]\nbranch = "update"\n',
		);
		const [diag] = diagnostics;

		expect(diagnostics).toHaveLength(1);
		expect(diag?.severity).toBe(vscode.DiagnosticSeverity.Information);
		expect(diag?.code).toBe('same-commit');
		expect(diag?.message).toBe('actions/checkout@v1: pin available, same commit: branch v1 = v1.0.5 (eeeeeee)');
		const actions = new ActupCodeActionProvider().provideCodeActions(
			document,
			document.lineAt(0).range,
			codeActionContext(diagnostics),
		);
		expect(actions.map(action => action.title)).toEqual(['Pin to eeeeeee # v1']);
		expect(editsOf(actions[0])).toEqual([{ range: rangeOf(document, 3, 'v1'), newText: `${head} # v1` }]);
	});

	test('a ref the version list lacks is a floating ref or a ref to pin', async () => {
		const unlisted: wasm.ActupProvider = {
			...provider,
			getVersions: () =>
				Promise.resolve({
					semverTags: [{ version: 'v1.0.0', sha: 'b'.repeat(40) }],
					defaultBranch: { name: 'main', sha: 'd'.repeat(40) },
				}),
		};
		const warn = await diagnoseRef('unlisted-warn.yml', 'stable', unlisted, '[policy]\nbranch = "warn"\n');
		const update = await diagnoseRef('unlisted-update.yml', 'stable', unlisted, '[policy]\nbranch = "update"\n');

		expect(warn.diagnostics.map(diag => [diag.code, diag.message])).toEqual([
			['floating', "actions/checkout@stable: 'stable' is a floating ref, pin to a tag or SHA"],
		]);
		expect(update.diagnostics.map(diag => [diag.code, diag.message])).toEqual([
			['pin-branch', "actions/checkout@stable: pin ref 'stable' to aaaaaaa # stable"],
		]);
	});
});
