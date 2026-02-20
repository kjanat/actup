(function_call
  function: (identifier) @_fn
  (#eq? @_fn "fromJSON")
  arguments: (arguments
    .
    (string
      (string_content) @injection.content
      (#set! injection.language "json"))))

(function_call
  function: (identifier) @_fn
  (#eq? @_fn "hashFiles")
  arguments: (arguments
    (string
      (string_content) @injection.content
      (#set! injection.language "gitignore"))))
