import type * as vscode from "vscode";

const DEBOUNCE_MS = 300;

/** Callback invoked when a document needs re-analysis. */
export type AnalysisCallback = (uri: vscode.Uri, text: string) => void;

/** Tracks open documents and debounces change events. */
export class DocumentTracker {
	private readonly timers = new Map<string, ReturnType<typeof setTimeout>>();

	constructor(private readonly onNeedsAnalysis: AnalysisCallback) {}

	/** Called when a matching document is opened. Triggers immediate analysis. */
	open(doc: vscode.TextDocument): void {
		this.onNeedsAnalysis(doc.uri, doc.getText());
	}

	/** Called when a matching document changes. Debounces re-analysis. */
	change(doc: vscode.TextDocument): void {
		const key = doc.uri.toString();
		const prev = this.timers.get(key);
		if (prev !== undefined) {
			clearTimeout(prev);
		}

		this.timers.set(
			key,
			setTimeout(() => {
				this.timers.delete(key);
				this.onNeedsAnalysis(doc.uri, doc.getText());
			}, DEBOUNCE_MS),
		);
	}

	/** Called when a document is closed. Cancels pending analysis. */
	close(uri: vscode.Uri): void {
		const key = uri.toString();
		const prev = this.timers.get(key);
		if (prev !== undefined) {
			clearTimeout(prev);
			this.timers.delete(key);
		}
	}

	/** Check if a document is a GitHub Actions file worth analyzing. */
	static isActionDocument(doc: vscode.TextDocument): boolean {
		if (doc.languageId === "github-actions-workflow") return true;

		const p = doc.uri.path;
		if (/\.github[/\\]workflows[/\\].*\.ya?ml$/.test(p)) return true;
		if (/[/\\]action\.ya?ml$/.test(p)) return true;

		return false;
	}
}
