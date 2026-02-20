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
    "contains" "startsWith" "endsWith" "format" "join" "toJSON" "fromJSON" "hashFiles" "success"
    "always" "cancelled" "failure"))

(function_call
  [
    "("
    ")"
  ] @punctuation.bracket)

"," @punctuation.delimiter

(delimited_expression
  [
    "${{"
    "}}"
  ] @punctuation.bracket)

(logical_group
  [
    "("
    ")"
  ] @punctuation.bracket)

[
  (operator)
  (not)
] @operator
