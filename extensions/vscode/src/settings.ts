import * as vscode from 'vscode';

/** The section and key of the explicit config file setting. */
export const CONFIG_PATH_SETTING = 'actup.configPath';

/** The `actup.configPath` setting, or `undefined` when it is unset or blank. */
export function configPathSetting(): string | undefined {
	const value = vscode.workspace.getConfiguration('actup').get<string | null>('configPath', null);
	return value === null || value.trim() === '' ? undefined : value;
}

/** Whether a settings change needs a new `actup-lsp` process. */
export function restartsServer(change: vscode.ConfigurationChangeEvent): boolean {
	return change.affectsConfiguration('actup.server') || change.affectsConfiguration(CONFIG_PATH_SETTING);
}

/** The `actup-lsp` initialization options that carry `configPath`. */
export function initializationOptions(configPath: string | undefined): { readonly configPath?: string } {
	return configPath === undefined ? {} : { configPath };
}
