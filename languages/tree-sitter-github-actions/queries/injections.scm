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

; A step's `shell:` sets the language of its `run:`. These come first so they
; claim the block before the bash default below. One rule per shell family so
; the injected name matches the host's registered language (`pwsh`/`powershell`
; -> powershell, `python` -> python). `cmd` has no quality grammar; it falls
; through to bash.

(block_mapping
 (block_mapping_pair
  key: (flow_node (plain_scalar (string_scalar) @_shell))
  value: (flow_node (plain_scalar (string_scalar) @_shellval)))
 (block_mapping_pair
  key: (flow_node (plain_scalar (string_scalar) @_run))
  value: [
   (block_node (block_scalar) @injection.content)
   (flow_node (plain_scalar) @injection.content)
  ])
 (#eq? @_shell "shell")
 (#match? @_shellval "^(pwsh|powershell)$")
 (#eq? @_run "run")
 (#set! injection.language "powershell"))

(block_mapping
 (block_mapping_pair
  key: (flow_node (plain_scalar (string_scalar) @_shell))
  value: (flow_node (plain_scalar (string_scalar) @_shellval)))
 (block_mapping_pair
  key: (flow_node (plain_scalar (string_scalar) @_run))
  value: [
   (block_node (block_scalar) @injection.content)
   (flow_node (plain_scalar) @injection.content)
  ])
 (#eq? @_shell "shell")
 (#eq? @_shellval "python")
 (#eq? @_run "run")
 (#set! injection.language "python"))

; `run:` with no overriding `shell:` uses the default shell (bash/sh). Covers
; both the `run: |` block form and the `run: one-liner` form.

(block_mapping_pair
 key: (flow_node (plain_scalar (string_scalar) @_run))
 value: [
  (block_node (block_scalar) @injection.content)
  (flow_node (plain_scalar) @injection.content)
 ]
 (#eq? @_run "run")
 (#set! injection.language "bash"))

; `actions/github-script` runs JavaScript from its `with.script:` input. Scope
; the injection to that step so a plain `script:` key elsewhere is untouched.

(block_mapping
 (block_mapping_pair
  key: (flow_node (plain_scalar (string_scalar) @_uses))
  value: (flow_node (plain_scalar (string_scalar) @_action)))
 (block_mapping_pair
  key: (flow_node (plain_scalar (string_scalar) @_with))
  value: (block_node (block_mapping
   (block_mapping_pair
    key: (flow_node (plain_scalar (string_scalar) @_script))
    value: (block_node (block_scalar) @injection.content)))))
 (#eq? @_uses "uses")
 (#match? @_action "^actions/github-script@")
 (#eq? @_with "with")
 (#eq? @_script "script")
 (#set! injection.language "javascript"))
