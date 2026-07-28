(comment) @comment
(string) @string
(escape_sequence) @string.escape
(integer) @number
(number) @number
(boolean) @boolean
(null) @constant

[
  "return"
  "for"
  "in"
  "boundary"
  "retry"
  "catch"
  "if"
  "else"
  "fold"
  "skip"
  "assert"
  "fail"
] @keyword

["and" "or" "not"] @operator
["=" "==" "!=" "<" "<=" ">" ">=" "+" "-" "*" "/" "%"] @operator

(binding_statement name: (identifier) @variable)
(for_expression binding: (identifier) @variable)
(fold_expression accumulator: (identifier) @variable)
(fold_expression binding: (identifier) @variable)
(boundary_expression error: (identifier) @variable)
(object_item key: (field_name (identifier) @property))
(member_expression property: (field_name) @property)
(call_expression function: (identifier) @function)
(call_expression function: (member_expression property: (field_name) @function))

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," ";" ":" "."] @punctuation.delimiter
