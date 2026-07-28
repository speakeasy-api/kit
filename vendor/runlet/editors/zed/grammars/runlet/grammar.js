/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const PREC = {
  conditional: 1,
  or: 2,
  and: 3,
  equality: 4,
  comparison: 5,
  additive: 6,
  multiplicative: 7,
  unary: 8,
  postfix: 9,
};

module.exports = grammar({
  name: "runlet",

  extras: ($) => [/\s/, $.comment],
  word: ($) => $.identifier,

  rules: {
    source_file: ($) =>
      seq(repeat(choice($.binding_statement, $.assert_statement)), $.return_statement),

    binding_statement: ($) =>
      seq(field("name", $.identifier), "=", field("value", $._expression), optional(";")),

    return_statement: ($) => seq("return", $._expression, optional(";")),

    block: ($) =>
      seq("{", repeat(choice($.binding_statement, $.assert_statement)), $.return_statement, "}"),

    assert_statement: ($) =>
      seq(
        "assert",
        "(",
        field("condition", $._expression),
        optional(seq(",", field("message", $._expression))),
        ")",
        optional(";"),
      ),

    _expression: ($) =>
      choice(
        $.conditional_expression,
        $.binary_expression,
        $.unary_expression,
        $.member_expression,
        $.index_expression,
        $.call_expression,
        $.for_expression,
        $.boundary_expression,
        $._primary_expression,
      ),

    conditional_expression: ($) =>
      prec.right(
        PREC.conditional,
        seq(
          field("consequence", $._expression),
          "if",
          field("condition", $._expression),
          "else",
          field("alternative", $._expression),
        ),
      ),

    binary_expression: ($) => {
      const table = [
        [PREC.or, "or"],
        [PREC.and, "and"],
        [PREC.equality, choice("==", "!=")],
        [PREC.comparison, choice("<", "<=", ">", ">=", "in")],
        [PREC.additive, choice("+", "-")],
        [PREC.multiplicative, choice("*", "/", "%")],
      ];

      return choice(
        ...table.map(([precedence, operator]) =>
          prec.left(
            /** @type {number} */ (precedence),
            seq(
              field("left", $._expression),
              operator,
              field("right", $._expression),
            ),
          ),
        ),
      );
    },

    unary_expression: ($) =>
      prec(PREC.unary, seq(choice("-", "not"), $._expression)),

    member_expression: ($) =>
      prec.left(
        PREC.postfix,
        seq(field("object", $._expression), ".", field("property", $.field_name)),
      ),

    index_expression: ($) =>
      prec.left(
        PREC.postfix,
        seq(field("object", $._expression), "[", field("index", $._expression), "]"),
      ),

    call_expression: ($) =>
      prec.left(
        PREC.postfix,
        seq(
          field("function", $._expression),
          "(",
          optional(seq($._expression, repeat(seq(",", $._expression)), optional(","))),
          ")",
        ),
      ),

    for_expression: ($) =>
      seq(
        "for",
        field("binding", $.identifier),
        "in",
        field("collection", $._expression),
        field("body", $.block),
      ),

    boundary_expression: ($) =>
      seq(
        "boundary",
        optional(seq("retry", field("retries", $.integer))),
        field("body", $.block),
        "catch",
        field("error", $.identifier),
        field("handler", $.block),
      ),

    _primary_expression: ($) =>
      choice(
        $.null,
        $.boolean,
        $.integer,
        $.number,
        $.string,
        $.identifier,
        $.list,
        $.object,
        $.parenthesized_expression,
      ),

    parenthesized_expression: ($) => seq("(", $._expression, ")"),

    list: ($) =>
      seq(
        "[",
        optional(seq($._expression, repeat(seq(",", $._expression)), optional(","))),
        "]",
      ),

    object: ($) =>
      seq(
        "{",
        optional(seq($.object_item, repeat(seq(",", $.object_item)), optional(","))),
        "}",
      ),

    object_item: ($) =>
      choice(
        seq(field("key", choice($.field_name, $.string)), ":", field("value", $._expression)),
        field("shorthand", $.identifier),
      ),

    field_name: ($) =>
      choice(
        $.identifier,
        "return", "for", "in", "boundary", "retry", "catch", "if", "else",
        "assert", "and", "or", "not", "null", "true", "false",
      ),
    identifier: (_) => /[_\p{L}][_\p{L}\p{M}\p{N}]*/,
    null: (_) => "null",
    boolean: (_) => choice("true", "false"),
    integer: (_) => /[0-9]+/,
    number: (_) =>
      token(prec(1, choice(/[0-9]+\.[0-9]+([eE][+-]?[0-9]+)?/, /[0-9]+[eE][+-]?[0-9]+/))),
    string: ($) =>
      seq('"', repeat(choice(token.immediate(/[^"\\\n]+/), $.escape_sequence)), '"'),
    escape_sequence: (_) => token.immediate(/\\(["\\\/bfnrt]|u[0-9a-fA-F]{4})/),
    comment: (_) => token(choice(seq("#", /.*/), seq("//", /.*/))),
  },
});
