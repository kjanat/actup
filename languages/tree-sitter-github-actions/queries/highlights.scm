(boolean_scalar) @boolean

(null_scalar) @constant.builtin

[
  (double_quote_scalar)
  (single_quote_scalar)
  (block_scalar)
  (string_scalar)
] @string

[
  (integer_scalar)
  (float_scalar)
] @number

(comment) @comment

[
  (anchor_name)
  (alias_name)
] @label

(tag) @type

[
  (yaml_directive)
  (tag_directive)
  (reserved_directive)
] @attribute

(block_mapping_pair
  key: (flow_node
    [
      (double_quote_scalar)
      (single_quote_scalar)
    ] @property))

(block_mapping_pair
  key: (flow_node
    (plain_scalar
      (string_scalar) @property)))

(flow_mapping
  (_
    key: (flow_node
      [
        (double_quote_scalar)
        (single_quote_scalar)
      ] @property)))

(flow_mapping
  (_
    key: (flow_node
      (plain_scalar
        (string_scalar) @property))))

[
  ","
  "-"
  ":"
  ">"
  "?"
  "|"
] @punctuation.delimiter

[
  "["
  "]"
  "{"
  "}"
] @punctuation.bracket

[
  "*"
  "&"
  "---"
  "..."
] @punctuation.special

(boolean) @boolean

(null) @constant.builtin

[
  (number)
  (format_index)
] @number

[
  (string)
  (format_string)
] @string

(format_variable
  [
    "{"
    "}"
  ] @punctuation.special)

(scape_sequence) @string.escape

(context
  (identifier) @variable.builtin
  (#any-of? @variable.builtin
    "github" "env" "vars" "job" "jobs" "steps" "runner" "secrets" "strategy" "matrix" "needs"
    "inputs"))

(property
  [
    (identifier)
    (asterisk)
  ] @variable.member)

(property_deref) @punctuation.delimiter

(index) @punctuation.delimiter

(function_call
  function: (identifier) @function.builtin
  (#any-of? @function.builtin
    "case" "contains" "startsWith" "endsWith" "format" "join" "toJSON" "fromJSON" "hashFiles" "success"
    "always" "cancelled" "failure"))

(function_call
  [
    "("
    ")"
  ] @punctuation.bracket)

(delimited_expression
  [
    (expression_marker)
    "}}"
  ] @punctuation.special)

(logical_group
  [
    "("
    ")"
  ] @punctuation.bracket)

[
  (operator)
  (not)
] @operator
