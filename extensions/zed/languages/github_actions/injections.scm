; Inject gh_actions_expressions into block mapping values containing ${{ ... }}
; Matches: plain scalars, double-quoted scalars, and block scalars
((block_mapping_pair
  value: [
    (block_node
      (block_scalar) @_value)
    (flow_node
      [
        (plain_scalar
          (string_scalar) @_value)
        (double_quote_scalar) @_value
      ])
  ]
  (#match? @_value "\\$\\{\\{")) @injection.content
  (#set! injection.language "GitHub Actions Expressions")
  (#set! injection.include-children))

; Inject gh_actions_expressions into flow mapping values containing ${{ ... }}
; Handles: { KEY: "${{ expr }}" } and { KEY: ${{ expr }} }
((flow_pair
  value: (flow_node
    [
      (plain_scalar
        (string_scalar) @_value)
      (double_quote_scalar) @_value
      (single_quote_scalar) @_value
    ]
    (#match? @_value "\\$\\{\\{"))) @injection.content
  (#set! injection.language "GitHub Actions Expressions")
  (#set! injection.include-children))

; Inject JavaScript into `script:` values when the step uses `actions/github-script`.
; Both `uses:` and `with.script:` must exist as siblings within the same step mapping.
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_uses_key
    value: (flow_node
      [
        (plain_scalar (string_scalar) @_uses_val)
        (double_quote_scalar) @_uses_val
        (single_quote_scalar) @_uses_val
      ]))
  (block_mapping_pair
    key: (flow_node) @_with_key
    value: (block_node
      (block_mapping
        (block_mapping_pair
          key: (flow_node) @_script_key
          value: (block_node
            (block_scalar) @injection.content)))))
  (#eq? @_uses_key "uses")
  (#match? @_uses_val "actions/github-script@")
  (#eq? @_with_key "with")
  (#eq? @_script_key "script")
  (#set! injection.language "javascript"))

; --- Shell overrides for `run:` based on sibling `shell:` field ---
; These rules must come BEFORE the default bash fallback below — tree-sitter
; injection precedence is "first match wins" within the same query file.
; Each rule requires both `run:` and `shell:` as siblings under the same step
; mapping (block_mapping). The `shell:` value can be plain, double-quoted,
; or single-quoted.

; PowerShell Core (`shell: pwsh`) — block scalar form
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_run_key
    value: (block_node
      (block_scalar) @injection.content))
  (block_mapping_pair
    key: (flow_node) @_shell_key
    value: (flow_node
      [
        (plain_scalar (string_scalar) @_shell_val)
        (double_quote_scalar) @_shell_val
        (single_quote_scalar) @_shell_val
      ]))
  (#eq? @_run_key "run")
  (#eq? @_shell_key "shell")
  (#match? @_shell_val "^[\"']?(pwsh|powershell)[\"']?$")
  (#set! injection.language "powershell"))

; PowerShell Core (`shell: pwsh`) — plain/quoted scalar (single-line) form
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_run_key
    value: (flow_node
      [
        (plain_scalar (string_scalar) @injection.content)
        (double_quote_scalar) @injection.content
        (single_quote_scalar) @injection.content
      ]))
  (block_mapping_pair
    key: (flow_node) @_shell_key
    value: (flow_node
      [
        (plain_scalar (string_scalar) @_shell_val)
        (double_quote_scalar) @_shell_val
        (single_quote_scalar) @_shell_val
      ]))
  (#eq? @_run_key "run")
  (#eq? @_shell_key "shell")
  (#match? @_shell_val "^[\"']?(pwsh|powershell)[\"']?$")
  (#set! injection.language "powershell"))

; Python (`shell: python`) — block scalar form
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_run_key
    value: (block_node
      (block_scalar) @injection.content))
  (block_mapping_pair
    key: (flow_node) @_shell_key
    value: (flow_node
      [
        (plain_scalar (string_scalar) @_shell_val)
        (double_quote_scalar) @_shell_val
        (single_quote_scalar) @_shell_val
      ]))
  (#eq? @_run_key "run")
  (#eq? @_shell_key "shell")
  (#match? @_shell_val "^[\"']?python[\"']?$")
  (#set! injection.language "python"))

; Python (`shell: python`) — plain/quoted scalar form
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_run_key
    value: (flow_node
      [
        (plain_scalar (string_scalar) @injection.content)
        (double_quote_scalar) @injection.content
        (single_quote_scalar) @injection.content
      ]))
  (block_mapping_pair
    key: (flow_node) @_shell_key
    value: (flow_node
      [
        (plain_scalar (string_scalar) @_shell_val)
        (double_quote_scalar) @_shell_val
        (single_quote_scalar) @_shell_val
      ]))
  (#eq? @_run_key "run")
  (#eq? @_shell_key "shell")
  (#match? @_shell_val "^[\"']?python[\"']?$")
  (#set! injection.language "python"))

; Windows CMD (`shell: cmd`) — block scalar form. Highlight as batch/bat.
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_run_key
    value: (block_node
      (block_scalar) @injection.content))
  (block_mapping_pair
    key: (flow_node) @_shell_key
    value: (flow_node
      [
        (plain_scalar (string_scalar) @_shell_val)
        (double_quote_scalar) @_shell_val
        (single_quote_scalar) @_shell_val
      ]))
  (#eq? @_run_key "run")
  (#eq? @_shell_key "shell")
  (#match? @_shell_val "^[\"']?cmd[\"']?$")
  (#set! injection.language "batch"))

; Windows CMD (`shell: cmd`) — plain/quoted scalar form
(block_mapping
  (block_mapping_pair
    key: (flow_node) @_run_key
    value: (flow_node
      [
        (plain_scalar (string_scalar) @injection.content)
        (double_quote_scalar) @injection.content
        (single_quote_scalar) @injection.content
      ]))
  (block_mapping_pair
    key: (flow_node) @_shell_key
    value: (flow_node
      [
        (plain_scalar (string_scalar) @_shell_val)
        (double_quote_scalar) @_shell_val
        (single_quote_scalar) @_shell_val
      ]))
  (#eq? @_run_key "run")
  (#eq? @_shell_key "shell")
  (#match? @_shell_val "^[\"']?cmd[\"']?$")
  (#set! injection.language "batch"))

; --- Default bash fallback for `run:` (no shell: sibling, or shell is bash/sh) ---

; Bash default — block scalar form (`run: |` and `run: >`)
((block_mapping_pair
  key: (flow_node) @_run_key
  value: (block_node
    (block_scalar) @injection.content))
  (#eq? @_run_key "run")
  (#set! injection.language "bash"))

; Bash default — plain/quoted scalar (single-line) form (`run: echo hi`)
((block_mapping_pair
  key: (flow_node) @_run_key
  value: (flow_node
    [
      (plain_scalar (string_scalar) @injection.content)
      (double_quote_scalar) @injection.content
      (single_quote_scalar) @injection.content
    ]))
  (#eq? @_run_key "run")
  (#set! injection.language "bash"))

; Inject gh_actions_expressions into bare `if:` values (no ${{ }} delimiters)
; GitHub Actions evaluates `if:` values as expressions even without ${{ }}
((block_mapping_pair
  key: (flow_node) @_key
  (#eq? @_key "if")
  value: (flow_node
    (plain_scalar
      (string_scalar) @_value)
    (#not-match? @_value "\\$\\{\\{"))) @injection.content
  (#set! injection.language "GitHub Actions Expressions")
  (#set! injection.include-children))
