/// <reference path="../node_modules/tree-sitter-cli/dsl.d.ts" />

declare function grammar<RuleName extends string>(
	options: Grammar<RuleName>,
): { grammar: GrammarSchema<RuleName> };

declare function grammar<
	BaseGrammarRuleName extends string,
	RuleName extends string,
>(
	baseGrammar: GrammarSchema<BaseGrammarRuleName>,
	options: Grammar<RuleName, BaseGrammarRuleName>,
): { grammar: GrammarSchema<RuleName | BaseGrammarRuleName> };
