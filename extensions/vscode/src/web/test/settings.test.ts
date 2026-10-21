import { beforeEach, describe, expect, test } from 'bun:test';
import { configPathSetting, initializationOptions, restartsServer } from '../../settings.ts';
import { workspace as mockWorkspace } from './vscode.ts';

beforeEach(() => mockWorkspace.reset());

describe('the desktop actup.configPath setting', () => {
	test('reaches the LSP initialization options when set', () => {
		mockWorkspace.settings.set('actup.configPath', 'ci/actup.toml');

		expect(initializationOptions(configPathSetting())).toEqual({ configPath: 'ci/actup.toml' });
	});

	test('restarts the server for its own section and actup.configPath', () => {
		const change = (changed: string) => ({ affectsConfiguration: (section: string) => section === changed });

		expect(restartsServer(change('actup.server'))).toBe(true);
		expect(restartsServer(change('actup.configPath'))).toBe(true);
		expect(restartsServer(change('actup-lsp.trace.server'))).toBe(false);
	});

	test('stays out of the options when unset or blank', () => {
		expect(initializationOptions(configPathSetting())).toEqual({});
		mockWorkspace.settings.set('actup.configPath', null);
		expect(initializationOptions(configPathSetting())).toEqual({});
		mockWorkspace.settings.set('actup.configPath', ' ');
		expect(initializationOptions(configPathSetting())).toEqual({});
	});
});
