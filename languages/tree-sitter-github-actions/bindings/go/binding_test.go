package tree_sitter_github_actions_test

import (
	"testing"

	tree_sitter_github_actions "github.com/kjanat/actup-v2/languages/tree-sitter-github-actions/bindings/go"
	tree_sitter "github.com/tree-sitter/go-tree-sitter"
)

func TestCanLoadGrammar(t *testing.T) {
	language := tree_sitter.NewLanguage(tree_sitter_github_actions.Language())
	if language == nil {
		t.Errorf("Error loading Github Actions grammar")
	}
}
