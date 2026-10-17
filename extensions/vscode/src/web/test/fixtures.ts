import type { ActupProvider, VersionTag } from "actup-wasm";
import * as vscode from "vscode";

export class FakeDocument implements vscode.TextDocument {
	readonly uri: vscode.Uri;
	readonly fileName: string;
	readonly isUntitled = false;
	readonly languageId = "github-actions-workflow";
	readonly encoding = "utf8";
	readonly version = 1;
	readonly isDirty = false;
	readonly isClosed = false;
	readonly eol = vscode.EndOfLine.LF;
	private readonly lines: string[];

	constructor(name: string, private readonly text: string) {
		this.uri = vscode.Uri.parse(`file:///${name}`);
		this.fileName = name;
		this.lines = text.split("\n");
	}

	get lineCount(): number {
		return this.lines.length;
	}

	save(): Thenable<boolean> {
		throw new Error("FakeDocument cannot save");
	}

	lineAt(lineOrPosition: number | vscode.Position): vscode.TextLine {
		const lineNumber = typeof lineOrPosition === "number" ? lineOrPosition : lineOrPosition.line;
		const text = this.lines[lineNumber];
		if (text === undefined) {
			throw new RangeError(`line ${lineNumber} is outside the document`);
		}
		const firstNonWhitespace = text.search(/\S/);
		return {
			lineNumber,
			text,
			range: new vscode.Range(lineNumber, 0, lineNumber, text.length),
			rangeIncludingLineBreak: new vscode.Range(lineNumber, 0, lineNumber + 1, 0),
			firstNonWhitespaceCharacterIndex: firstNonWhitespace === -1 ? text.length : firstNonWhitespace,
			isEmptyOrWhitespace: firstNonWhitespace === -1,
		};
	}

	offsetAt(position: vscode.Position): number {
		return this.lines
			.slice(0, position.line)
			.reduce((offset, line) => offset + line.length + 1, position.character);
	}

	positionAt(offset: number): vscode.Position {
		let remaining = offset;
		for (const [line, text] of this.lines.entries()) {
			if (remaining <= text.length) {
				return new vscode.Position(line, remaining);
			}
			remaining -= text.length + 1;
		}
		return new vscode.Position(this.lines.length - 1, this.lines.at(-1)?.length ?? 0);
	}

	getText(range?: vscode.Range): string {
		if (range === undefined) {
			return this.text;
		}
		return this.text.slice(this.offsetAt(range.start), this.offsetAt(range.end));
	}

	getWordRangeAtPosition(): vscode.Range | undefined {
		throw new Error("FakeDocument has no word ranges");
	}

	validateRange(range: vscode.Range): vscode.Range {
		return range;
	}

	validatePosition(position: vscode.Position): vscode.Position {
		return position;
	}
}

export function fakeProvider(tagsByRepo: Readonly<Record<string, VersionTag[]>>): ActupProvider {
	return {
		getVersions: (_host, owner, repo) => {
			const semverTags = tagsByRepo[`${owner}/${repo}`];
			return semverTags === undefined
				? Promise.reject(new Error(`no repository ${owner}/${repo}`))
				: Promise.resolve({ semverTags });
		},
		resolveRefToSha: () => Promise.resolve("a".repeat(40)),
		getDefaultBranch: () => Promise.resolve("main"),
		getReadme: () => Promise.resolve(null),
		getActionYml: () =>
			Promise.resolve({
				raw: "",
				name: null,
				description: null,
				author: null,
				inputs: {},
				outputs: {},
				runs: null,
			}),
	};
}

export function rangeOf(document: vscode.TextDocument, line: number, needle: string): vscode.Range {
	const start = document.lineAt(line).text.indexOf(needle);
	if (start === -1) {
		throw new Error(`line ${line} has no ${needle}`);
	}
	return new vscode.Range(line, start, line, start + needle.length);
}

export function codeActionContext(diagnostics: readonly vscode.Diagnostic[]): vscode.CodeActionContext {
	return { diagnostics, triggerKind: vscode.CodeActionTriggerKind.Invoke, only: undefined };
}

export function editsOf(action: vscode.CodeAction | undefined): { range: vscode.Range; newText: string }[] {
	return (action?.edit?.entries() ?? []).flatMap(([, edits]) => edits.map(({ range, newText }) => ({ range, newText })));
}
