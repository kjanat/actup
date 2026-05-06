/**
 * @file Github Actions grammar for tree-sitter
 * @author Kaj Kowalski <info@kajkowalski.nl>
 * @license MIT
 */

/// <reference path="./node_modules/tree-sitter-cli/dsl.d.ts" />
// @ts-check

export default grammar({
	name: "github_actions",

	rules: {
		// TODO: add the actual grammar rules
		source_file: $ => "hello",
	},
});
