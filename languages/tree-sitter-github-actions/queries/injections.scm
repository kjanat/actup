; Inject `gh_actions_expressions` into any scalar that contains a
; GitHub Actions expression (`${{ … }}`).
;
; The expression grammar is responsible for finding the `${{ … }}` blocks
; within the scalar text — we hand it the whole node so it sees both the
; literal prefix/suffix and the expression(s).

((plain_scalar) @injection.content
 (#match? @injection.content "\\$\\{\\{")
 (#set! injection.language "gh_actions_expressions")
 (#set! injection.include-children))

((single_quote_scalar) @injection.content
 (#match? @injection.content "\\$\\{\\{")
 (#set! injection.language "gh_actions_expressions")
 (#set! injection.include-children))

((double_quote_scalar) @injection.content
 (#match? @injection.content "\\$\\{\\{")
 (#set! injection.language "gh_actions_expressions")
 (#set! injection.include-children))

((block_scalar) @injection.content
 (#match? @injection.content "\\$\\{\\{")
 (#set! injection.language "gh_actions_expressions")
 (#set! injection.include-children))
