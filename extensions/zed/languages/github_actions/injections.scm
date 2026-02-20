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
