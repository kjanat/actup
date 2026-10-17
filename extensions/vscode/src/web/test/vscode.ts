export class Position {
	constructor(readonly line: number, readonly character: number) {}
}

export class Range {
	readonly start: Position;
	readonly end: Position;

	constructor(startLine: number, startCharacter: number, endLine: number, endCharacter: number) {
		this.start = new Position(startLine, startCharacter);
		this.end = new Position(endLine, endCharacter);
	}
}

export class Uri {
	private constructor(private readonly value: string) {}

	static parse(value: string): Uri {
		return new Uri(value);
	}

	toString(): string {
		return this.value;
	}
}

export enum EndOfLine {
	LF = 1,
	CRLF = 2,
}

export enum DiagnosticSeverity {
	Error = 0,
	Warning = 1,
	Information = 2,
	Hint = 3,
}

export class Diagnostic {
	source?: string;
	code?: string | number;

	constructor(readonly range: Range, readonly message: string, readonly severity: DiagnosticSeverity) {}
}

export class CodeActionKind {
	static readonly QuickFix = new CodeActionKind("quickfix");
	static readonly SourceFixAll = new CodeActionKind("source.fixAll");

	constructor(readonly value: string) {}

	append(part: string): CodeActionKind {
		return new CodeActionKind(`${this.value}.${part}`);
	}
}

export enum CodeActionTriggerKind {
	Invoke = 1,
	Automatic = 2,
}

export class TextEdit {
	constructor(readonly range: Range, readonly newText: string) {}
}

export class WorkspaceEdit {
	private readonly edits = new Map<string, { uri: Uri; edits: TextEdit[] }>();

	replace(uri: Uri, range: Range, newText: string): void {
		const key = uri.toString();
		const entry = this.edits.get(key) ?? { uri, edits: [] };
		entry.edits.push(new TextEdit(range, newText));
		this.edits.set(key, entry);
	}

	entries(): [Uri, TextEdit[]][] {
		return [...this.edits.values()].map(({ uri, edits }) => [uri, edits]);
	}
}

export class CodeAction {
	edit?: WorkspaceEdit;
	diagnostics?: Diagnostic[];
	isPreferred?: boolean;

	constructor(readonly title: string, readonly kind?: CodeActionKind) {}
}

export enum CompletionItemKind {
	Value = 11,
	Reference = 17,
}

export enum CompletionTriggerKind {
	Invoke = 0,
	TriggerCharacter = 1,
	TriggerForIncompleteCompletions = 2,
}

export class CompletionItem {
	detail?: string;
	insertText?: string;
	range?: Range;
	sortText?: string;

	constructor(readonly label: string, readonly kind?: CompletionItemKind) {}
}

export class CompletionList {
	constructor(readonly items: CompletionItem[] = [], readonly isIncomplete = false) {}
}

export enum LogLevel {
	Off = 0,
	Trace = 1,
	Debug = 2,
	Info = 3,
	Warning = 4,
	Error = 5,
}

export class MarkdownString {
	value = "";
	isTrusted?: boolean;

	appendMarkdown(value: string): MarkdownString {
		this.value += value;
		return this;
	}
}

export class Hover {
	readonly contents: MarkdownString[];

	constructor(contents: MarkdownString | MarkdownString[], readonly range?: Range) {
		this.contents = Array.isArray(contents) ? contents : [contents];
	}
}

interface UriLike {
	toString(): string;
}

interface OpenDocument {
	readonly uri: UriLike;
	getText(): string;
	lineAt(line: number): { readonly text: string };
}

class Workspace {
	readonly documents: OpenDocument[] = [];
	readonly appliedEdits: WorkspaceEdit[] = [];

	findFiles(include: string): Promise<UriLike[]> {
		const directory = include.includes("/workflows/") ? "/.github/workflows/" : "/.github/actions/";
		return Promise.resolve(
			this.documents.filter(document => document.uri.toString().includes(directory)).map(document => document.uri),
		);
	}

	openTextDocument(uri: UriLike): Promise<OpenDocument> {
		const document = this.documents.find(candidate => candidate.uri.toString() === uri.toString());
		return document === undefined ? Promise.reject(new Error(`no document ${uri.toString()}`)) : Promise.resolve(document);
	}

	applyEdit(edit: WorkspaceEdit): Promise<boolean> {
		this.appliedEdits.push(edit);
		return Promise.resolve(true);
	}

	reset(): void {
		this.documents.splice(0);
		this.appliedEdits.splice(0);
	}
}

export const workspace = new Workspace();

class Window {
	readonly messages: string[] = [];

	showInformationMessage(message: string): Promise<undefined> {
		this.messages.push(message);
		return Promise.resolve(undefined);
	}

	showWarningMessage(message: string): Promise<undefined> {
		this.messages.push(message);
		return Promise.resolve(undefined);
	}

	showErrorMessage(message: string): Promise<undefined> {
		this.messages.push(message);
		return Promise.resolve(undefined);
	}

	reset(): void {
		this.messages.splice(0);
	}
}

export const window = new Window();

class DiagnosticCollection {
	private readonly byUri = new Map<string, Diagnostic[]>();

	constructor(readonly name: string) {}

	set(uri: Uri, diagnostics: readonly Diagnostic[]): void {
		this.byUri.set(uri.toString(), [...diagnostics]);
	}

	get(uri: Uri): readonly Diagnostic[] | undefined {
		return this.byUri.get(uri.toString());
	}

	delete(uri: Uri): void {
		this.byUri.delete(uri.toString());
	}

	clear(): void {
		this.byUri.clear();
	}

	dispose(): void {
		collections.delete(this);
	}
}

const collections = new Set<DiagnosticCollection>();

export const languages = {
	createDiagnosticCollection(name: string): DiagnosticCollection {
		const collection = new DiagnosticCollection(name);
		collections.add(collection);
		return collection;
	},
	getDiagnostics(uri: Uri): Diagnostic[] {
		return [...collections].flatMap(collection => collection.get(uri) ?? []);
	},
};
