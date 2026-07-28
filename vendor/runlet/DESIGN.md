# Runlet: language and runtime design

Status: implementation-ready for the Phase 0 semantic executable model  
Audience: language implementers, runtime embedders, tool authors, and agent-harness authors

## 1. Executive summary

Runlet is a small, embedded orchestration language for agents. A Runlet program describes a dataflow graph whose effectful nodes are host-registered tool calls. Tool calls produce values that behave like ordinary values in source code but are represented internally as unresolved outputs. Reading fields, constructing objects, selecting branches, iterating, and passing values to later calls records dependencies rather than blocking the interpreter. The runtime schedules each call as soon as its inputs are ready, automatically exposing parallelism.

Runlet combines:

- a small expression language with familiar Python/Starlark-like statements and JavaScript-like object literals;
- Pulumi-style implicit output lifting, without an explicit `Output<T>` type;
- structured concurrency and dynamic graph expansion for `if`, `for`, and error boundaries;
- inspectable, event-driven execution graphs;
- durable execution based on an append-only journal and deterministic replay; and
- schema-derived static checking with no type syntax in user programs.

The language has no imports, packages, user-defined classes, macros, threads, explicit `await`, or user-visible type declarations. An embedder constructs a runtime with the complete set of names and schemas visible for that invocation. Progressive discovery is an application pattern: a host-provided search tool may return descriptors that the host registers in a subsequent runtime construction, but discovery is not special syntax or runtime mutation.

### 1.1 The complete user mental model (under 100 model tokens in common tokenizers)

> Tool calls return hidden future values. Calls run when inputs are ready; references create edges, so independent calls run in parallel. Only calls flowing into `return` run. `if` and `for` create dynamic graph nodes; the host bounds concurrent iterations. `assert` checks invariants without returning intermediate values. `boundary { ... } catch err { ... }` handles subgraph failures. `return` waits only for referenced values. Types and safe conversions come from tool schemas and are never written. Errors point to a fix.

### 1.2 Goals

1. Make correct parallel tool orchestration the default.
2. Keep the syntax and conceptual surface small enough for a capable model to learn from a short prompt.
3. Give agents compiler-quality diagnostics without asking them to write types.
4. Make every planned and running dependency visible to a UI.
5. Resume interrupted executions without repeating completed work.
6. Let embedders define almost the entire callable environment.
7. Keep the Rust runtime fast, memory-efficient, safe, and deterministic.

### 1.3 Non-goals for version 1

- General-purpose application programming or arbitrary computation.
- User-defined functions, recursion, modules, imports, classes, or metaprogramming.
- Distributed scheduling built into the core crate. The runtime exposes persistence and executor interfaces on which a distributed implementation can be built.
- Exactly-once external side effects. No general system can guarantee this against arbitrary tools; Runlet offers journaled at-most-once dispatch and idempotency keys, and documents the remaining uncertainty.
- Transparent migration of in-flight runs across arbitrary source or schema changes.
- A user-visible nominal type system.

## 2. Source language

### 2.1 Lexical rules

Source is UTF-8. Identifiers use Unicode XID start/continue rules; tool authors SHOULD expose simple ASCII identifiers. The reserved words are `return`, `for`, `in`, `boundary`, `fold`, `skip`, `assert`, `fail`, `retry`, `catch`, `if`, `else`, `and`, `or`, `not`, `null`, `true`, and `false`. They cannot be binding names or registry roots. A reserved word is permitted contextually as an object property name or after `.`, so `{ assert: true }` and `result.assert` remain natural; quoted/indexed spellings are also valid.

Indentation is not significant. A newline terminates a simple statement unless it occurs inside an open `()`, `[]`, or expression-object `{}`, or the preceding token is one of this exhaustive continuation set: `=`, `,`, `:`, `+`, `-`, `*`, `/`, `%`, `==`, `!=`, `<`, `<=`, `>`, `>=`, `and`, `or`, `not`, `in`, `if`, or `else`. Compound `for`, `boundary`, and `catch` headers must place their opening `{` on the header line; a boundary body's closing `}` and its `catch` must likewise be on one line as `} catch err {`. A newline immediately followed by `(` or `[` never continues the preceding expression; a call/index continuation must remain on the same line or wrap the whole expression in open parentheses. Semicolons may terminate simple statements and are optional immediately before `}`. Comments start with `#` or `//` and continue to end of line. Block comments are deliberately omitted.

Literals are:

- `null`, `true`, `false`;
- unsigned decimal integer tokens such as `12` (negative values such as `-4` are a unary operator applied to a literal);
- decimal floating-point values such as `1.25` and `6e4`;
- double-quoted strings with JSON escapes; and
- lists (`[a, b]`) and objects (`{ name: value, "non_identifier": value }`).

Numbers do not silently lose precision. Integer literals are arbitrary precision during analysis, then constrained by their use. Runtime integers are signed 64-bit; runtime floats are IEEE-754 binary64. A literal outside the accepted target range is a compile error.

### 2.2 Core grammar

The following EBNF is normative for the version 1 surface grammar. Whitespace, comments, and statement terminators are elided. There are deliberately no effectful expression statements, statement-form control structures, or early returns. `skip` is the one non-binding statement: it is loop-body control (section 2.7.1), not an expression.

```ebnf
program       = ( let_stmt | assert_stmt )* , return_stmt ;

let_stmt      = IDENT , "=" , expression ;
skip_stmt     = "skip" , ( "if" , conditional_or )? ;   (* for/fold bodies only *)
assert_stmt   = "assert" , "(" , expression , ( "," , expression )? , ")" ;
return_stmt   = "return" , expression ;

if_expr       = "if" , expression , block_return ,
                ( "else" , ( if_expr | block_return ) )? ;
for_expr      = "for" , IDENT , "in" , expression , block_return ;
fold_expr     = "fold" , IDENT , "=" , expression ,
                "for" , IDENT , "in" , expression , block_return ;
fail_expr     = "fail" , "(" , arguments , ")" ;

boundary_expr = "boundary" , retry_clause? , block_return , catch_clause_return ;
retry_clause  = "retry" , INTEGER ;
catch_clause_return = "catch" , IDENT , block_return ;
block_return  = "{" , ( let_stmt | skip_stmt | assert_stmt )* , return_stmt , "}" ;

expression    = conditional_or ,
                ( "if" , conditional_or , ( "else" , expression )? )? ;
conditional_or = conditional_and , ( "or" , conditional_and )* ;
conditional_and = equality , ( "and" , equality )* ;
equality      = comparison , ( ( "==" | "!=" ) , comparison )* ;
comparison    = additive , ( ( "<" | "<=" | ">" | ">=" | "in" ) , additive )* ;
additive      = multiplicative , ( ( "+" | "-" ) , multiplicative )* ;
multiplicative = unary , ( ( "*" | "/" | "%" ) , unary )* ;
unary         = ( "not" | "-" )* , postfix ;
postfix       = primary , ( member | index | call )* ;
member        = "." , FIELD_NAME ;
index         = "[" , expression , "]" ;
call          = "(" , arguments? , ")" ;
arguments     = expression , ( "," , expression )* , ","? ;
primary       = literal | IDENT | list | object | "(" , expression , ")"
              | if_expr | for_expr | fold_expr | fail_expr | boundary_expr ;
literal       = "null" | "true" | "false" | INTEGER | NUMBER | STRING ;
list          = "[" , ( expression , ( "," , expression )* , ","? )? , "]" ;
object        = "{" , ( object_item , ( "," , object_item )* , ","? )? , "}" ;
object_item   = ( FIELD_NAME | STRING ) , ":" , expression
              | "[" , expression , "]" , ":" , expression
              | IDENT ;
```

`FIELD_NAME` is an `IDENT` or any reserved word. Reserved words are not allowed by the object shorthand arm because no binding can have that name.

`{ [expression]: value }` computes a property key at runtime. The key expression must produce a String or a scalar (Integer, Number, Boolean); scalars convert to their canonical text form, so `{ [user.id]: user }` keys by `"42"`. Any other kind is `RL2315` when statically known and the catchable `RL5209` otherwise. When keys collide — computed or mixed with static — the last entry wins, matching `+` merge's right bias; repeating the same *static* key stays the compile error `RL2203` because writing it twice is always a mistake. A literal containing a computed key has map schema (its keys are unknowable statically) over the union of every entry's value schema. Computed keys plus `fold` express keyed accumulation — grouping, indexing, counting by key — without dedicated intrinsics: `fold acc = {} for u in users { return acc + { [u.id]: u } }`.

`{ customer, orders }` is shorthand for `{ customer: customer, orders: orders }`. Blocks and object literals are syntactically distinguishable from context. Assignment is declaration, not mutation: a name may be declared only once in a lexical scope. An inner scope may shadow an outer local name; a linter warns because accidental shadowing is usually harmful in generated programs. Registry root names are reserved in every scope: if `crm.customer` is registered, `crm = value` is a compile error with a rename fix. This makes callable-path resolution stable across local edits; a newly constructed progressive-disclosure registry can reject a previously valid source with a diagnostic naming the colliding root and registry digest.

The parser accepts a final expression only through `return`; implicit final-expression and early returns are intentionally omitted so missing or ambiguous output is diagnosed clearly. In recovery mode, familiar statement-form `if`, `for`, `boundary`, and standalone calls receive one construct-level diagnostic and a machine fix toward the corresponding expression/binding form rather than cascades of reachability errors. The `heal` pre-pass builds on those machine fixes: it applies safe, insertion-only repairs (missing `return`s, unbound statements, missing separators), re-parses, and hands hosts a healed source plus notes — an embedder can execute the repaired program and surface the notes as warnings instead of costing the model a retry.

### 2.3 Calls and namespaces

Every callable is supplied by the embedder. A name such as `crm.customer` is a path in an immutable registry, not a dynamic object lookup. Registry namespaces can be arbitrarily nested. The analyzer resolves the longest callable path before treating postfix members as value field access:

```runlet
customer = crm.customer(customer_id) # registry call
email = customer.email               # output field projection
```

Values cannot be invoked unless their schema explicitly identifies them as a host-callable handle. Version 1 SHOULD disable callable handles by default because stable serialization, authorization, and replay are harder than for named registry calls.

Tools may accept positional arguments, one object argument, or both if their schema describes both forms. A registry MUST reject overload sets whose accepted call shapes overlap; Runlet does not perform ad-hoc overload ranking.

Before validation, dispatch, or operation-identity hashing, every call is normalized to a canonical List containing converted arguments in source order. Thus `f(a, b)` hashes/dispatches `[a, b]`, while `f({ x: a })` hashes/dispatches `[{ x: a }]`; the latter is never silently flattened. `CallSchema` maps these list positions to declared parameters and the executor SDK may present a convenient unwrapped payload to the implementation, but the runtime `CanonicalValue` and RCVE identity always retain the list envelope. Version 1 has no variadic or keyword arguments.

Calls to host tools are effectful graph nodes. A small set of deterministic operations can be registered as intrinsics. Intrinsics share the call syntax and schema machinery but execute locally during graph evaluation. All logical state transitions are journaled; results of nondeterministic intrinsics are always persisted, while deterministic results may be marked re-derivable under section 5.3. The recommended default prelude is described in section 7.

### 2.4 Hidden outputs and lifting

The analyzer assigns every expression a hidden shape `Value<T>` or `Output<T>`. These are implementation concepts and never appear in source or diagnostics unless an internal-debug flag is enabled.

- A literal is a resolved `Value<T>`.
- A tool call is an `Output<T>`.
- A member/index operation on an output is an output projection.
- An operator or intrinsic whose operand is an output becomes a deterministic compute node and produces an output.
- A list or object containing any output becomes a composite output. Its independent leaves remain separately observable; constructing it does not serialize those leaves.
- Passing an output to a tool records a dependency. The tool becomes runnable only after all input leaves and required dynamic controls resolve.

There is no `await`. Source evaluation plans graph structure until it encounters a control decision that depends on unresolved data. The runtime installs a dynamic control node and continues expansion when that value resolves.

### 2.4.1 Normative operator semantics

Operators use the schema-directed conversions in section 3.3 only after selecting a unique applicable row. Equal-ranked ambiguity is an error. All chains containing more than one relational, membership, or equality operator—`==`, `!=`, `<`, `<=`, `>`, `>=`, or `in`, including mixed chains—are rejected with a fix such as `a < b and b < c`. They are not left-associative and do not have Python's implicit chaining semantics. The repeated grammar productions exist so the parser can recognize the whole mistaken chain and issue one diagnostic.

| Operator | Accepted operands | Result and failures |
|---|---|---|
| unary `-` | Integer or Number | Same numeric type; checked `RL5101 NUMERIC_OVERFLOW` |
| `+` | Integer×Integer; numeric pair; String×String; List×List; Object/Map×Object/Map | Checked Integer; Number after safe widening; concatenated String; ordered list with union element schema; shallow right-biased merge |
| `-`, `*` | Integer×Integer or numeric pair | Checked Integer or Number after safe widening |
| `/` | Numeric pair | Number; `RL5102 DIVISION_BY_ZERO`; each Integer must be exactly representable before widening |
| `%` | Integer×Integer | Integer with quotient truncated toward zero and remainder having the dividend's sign; zero divisor is `RL5102` |
| `==`, `!=` | Values with an exact/lossless common schema | Boolean structural equality; lists are order-sensitive, objects/maps compare key/value sets, and numeric comparison uses safe widening only |
| `<`, `<=`, `>`, `>=` | numeric pair or String×String | Boolean; strings compare Unicode scalar values lexicographically, not locale collation |
| `in` | value×List; String×Object/Map; String×String | Structural list membership; key presence; Unicode-scalar substring membership |
| `not` | Boolean | Boolean |
| `and`, `or` | Boolean×Boolean | Boolean with short-circuit dynamic expansion |

String concatenation provides a typed String context, so `"count: " + 3` uses canonical Integer-to-String conversion. Two operands for which multiple overload families would require equal-ranked conversions fail rather than guess. List concatenation never converts elements: `[1] + ["a"]` has element schema `Integer | String`.

Object merge is shallow and right-biased: `defaults + overrides` keeps every key of both sides and the right side's value wins on collision. Two literal objects merge to the exact combined object schema; when either side is a map, the result is a map over the union of both sides' value schemas. Deep merge is deliberately absent — recursion depth and list-versus-replace semantics are not resolvable by intuition, and explicit nesting (`base + { nested: base.nested + patch }`) states them.

All Integer arithmetic is checked i64 arithmetic. Literal-only overflow is a compile error; overflow involving runtime data is a catchable compute failure. Number operations use correctly rounded IEEE-754 binary64, but any non-finite result fails as `RL5103 NON_FINITE_NUMBER`. Operators are deterministic compute/branch nodes when any operand is unresolved.

### 2.4.2 Normative index and projection semantics

Indexing has these exact domains and results:

| Target | Index | Result |
|---|---|---|
| `List<T>` | Integer | `T` at the normalized element index |
| String | Integer | one Unicode scalar value encoded as a String, indexed by scalar position rather than UTF-8 byte |
| Bytes | Integer | Integer in `0..=255` at the byte position |
| `Object` | String | the statically known property schema for a literal key, or the normalized union of property schemas for a dynamic key |
| `Map<T>` | String | `T` for the matching key |
| `Any` | Integer or String | runtime dispatch to one of the rows above; otherwise `RL5203 NOT_INDEXABLE` |

For list, string, and bytes targets, a negative index is normalized as `length + index`, matching Python/Starlark (`-1` is the last element). A normalized index outside the half-open interval `[0, length)` fails with `RL5205 INDEX_OUT_OF_BOUNDS`; the error contains original/normalized index and length. A missing object/map key fails with `RL5206 KEY_NOT_FOUND` and closest keys, never returns implicit null. A statically known invalid target/index pair, missing literal property, or out-of-range literal index into a literal value is a compile error with the same explanation and an edit where possible. Runtime projection failures are catchable according to lexical boundary ownership.

Dot projection is equivalent to indexing by that literal field name after schema checking, except registry callable-path resolution takes precedence under section 2.3. Optional properties retain their nullable/optional schema when present in every variant; absence of an optional property at runtime is `RL5206`, so callers guard with membership when they want a default instead of a failure: `obj[key] if key in obj else fallback` (the conditional is lazy, so the missing-key projection never evaluates). Integer and String index contexts use the conversion ranks in section 3.3 only when a unique target row is statically selected.

### 2.5 Reachability and execution

Runlet is lazy with respect to pure work and eager with respect to declared effects. Pure tool and compute nodes are eligible to run only when transitively reachable from the program's returned value. Statements whose expressions contain a call to a tool with an effectful execution policy (anything other than `Pure`) are *implicit roots*: when their enclosing block runs, they evaluate in statement order before the block result, whether or not the result references them. This keeps `return waits only for values it references` precise for reads and transforms while guaranteeing that a fire-and-forget write the author bound is never silently dropped.

At initial planning, the runtime creates nodes and edges for all statically discoverable expressions. It then performs reachability from the root return node plus every effect-rooted statement of the executed block. Dynamic branch/loop expansion adds reachable nodes later; an effect binding inside a loop body roots once per iteration, and a postfix conditional around an effectful call still selects which branch dispatches. A failing effect root fails its block exactly like a referenced call, so `boundary` owns it.

After reachability analysis, an unused binding whose expression contains an effectful call receives no diagnostic — it runs. An unused binding containing only pure calls, literals, or deterministic local compute is pruned and receives warning `RL1205 UNUSED_BINDING`. Thus a pure `x = tool()` never silently dispatches, an effectful one never silently disappears, and both outcomes are visible: the first in diagnostics, the second in the execution graph. (Fatal `RL1204 UNREACHABLE_EFFECT` from earlier drafts is retired; the code is reserved.)

### 2.6 Conditional expressions

Runlet's primary conditional uses Python's conditional-expression shape:

```runlet
label = "large" if order.total >= 1000 else "standard"
```

The `else` arm is optional and defaults to `null`: `x if cond` is `x if cond else null`. Combined with null-omission for optional object properties (section 3.3) and implicit effect roots (section 2.5), this makes conditional writes and conditional properties first-class: `result = crm.update({ id: c.id }) if needs_fix` dispatches only when the condition holds and needs no explicit null arm.

For branches too large for a postfix conditional, `if` is also a block-bodied expression — the branching counterpart, not an alias: a postfix conditional cannot hold multi-statement branches at all. Each branch is a full block ending in `return`, `else if` chains compose, a missing `else` yields `null`, and only the selected branch evaluates — its implicit effect roots included, so writes inside a branch dispatch exactly when the branch is selected:

```runlet
grade = if score >= 90 {
    badge = badges.award({ user, kind: "gold" })
    return "gold"
} else if score >= 60 {
    return "silver"
} else {
    return "bronze"
}
```

There is still no statement-form `if`: the construct is an expression and its value is bound or returned (`RL1014` guides the rewrite, consuming the whole construct so one mistake yields one diagnostic).

If a condition is resolved while planning, only the selected expression is planned. If it is an output, the runtime creates a `Branch` node depending on that condition. Neither result expression's effectful nodes are scheduled until selection. When the condition resolves, only the selected expression is expanded. The untaken expression is visible in source metadata but has no runtime call nodes; the graph UI shows it as `not_materialized`, not `skipped`.

A condition must be Boolean. Runlet has no general truthiness conversion: empty strings, zero, empty lists, and null are not Booleans. If its inferred schema is `Any`, the analyzer inserts a runtime Boolean check; failure is a `RL5202` compute error naming the actual value kind and the no-truthiness rule, stamped with the condition's source span, owned and catchable according to the expression's enclosing boundary. This removes a common class of generated-code mistakes without making `Any` unusable.

Only the selected result is evaluated when its condition is unresolved. Conditional expressions and `boundary` expressions are the two version 1 mechanisms that merge alternative values into later dataflow. Nested right-associative conditional expressions express `else if` behavior.

### 2.7 `for`

`for` is expression-only and returns an ordered list:

```runlet
scores = for order in orders {
    return fraud.score(customer, order, limits)
}
```

The example syntax in the project brief omitted `return` within the loop body. Runlet deliberately requires it so multi-statement bodies are unambiguous and accidental last-expression changes cannot alter output. A parser diagnostic detects the omitted form and provides the one-token repair:

```text
RL1017: a `for` expression body must return a value
help: write `return fraud.score(customer, order, limits)`
```

The collection must resolve to a finite list, object, or map. Lists iterate in index order. Objects and maps iterate as `{ key, value }` records in lexicographically sorted UTF-8 key order, ensuring replay determinism. Streams and unbounded iterators are not in version 1.

Concurrency is host policy, configured with `RuntimeBuilder::loop_concurrency`.
Source programs cannot override it because agents do not know provider quotas,
runtime load, or tool safety constraints. The configured value bounds active
iteration scopes, not merely individual tool calls, and never means unbounded.

Result order matches input order, independent of completion order. A failed iteration fails the loop node and cancels its unfinished sibling iterations unless a boundary inside the body catches the failure. A boundary outside the loop treats the complete loop as part of its attempt.

Loop expansion is incremental: after the collection resolves, the runtime
materializes work according to the host concurrency policy and adds more as
capacity becomes available. Graph event consumers still receive stable virtual
iteration identities.

### 2.7.1 `skip`

`skip [if condition]` is the one statement that is not a binding. It is valid
only directly inside a `for` or `fold` body — never at program level, and
never inside a `boundary` block (a skip abandoning retry accounting mid-attempt
would be incoherent; the parser rejects it with `RL1018`). A taken skip ends
the current iteration: in `for`, the element is dropped from the loop result
(filtering); in `fold`, the accumulator passes through unchanged. Skip
conditions follow the same no-truthiness rule as all conditions and always
evaluate, in statement order interleaved with implicit effect roots, before
the block result. Making skips explicit (rather than treating a block that
falls off the end as "yields nothing") preserves totality checking: a body
path that neither returns nor skips is still a compile error, so a forgotten
`return` cannot silently shorten a result list.

### 2.7.2 `fold`

`fold` is the sequential counterpart to `for` and the single way to reduce:

```runlet
total = fold acc = 0 for order in orders {
    skip if order.status != "completed"
    return acc + order.amount
}
```

Each iteration binds a fresh accumulator (immutability is preserved — this is
rebinding, not mutation) and the body's return becomes the next accumulator.
An empty collection yields the initial value. Iterations are sequential by
definition, which is the point: `fold` is the construct that pins
order-dependence, exactly as `for` pins host-bounded concurrency and
`boundary` pins retry. Ordered effect chains and cursor pagination are
expressible only here.

The accumulator keeps one schema: the body result must convert back to the
initial value's schema (`RL2313`) — strict one-pass unification rather than
fixpoint widening, so errors stay legible. One widening is allowed: when the
initial value converts *structurally* (never by formatting) into the body's
schema, the accumulator adopts the body's schema. This covers the
keyed-accumulation idiom — an `{}` seed merging computed-key entries
(`acc + { [key]: value }`) widens to the map — and the find-first idiom — a
`null` seed with `return item if match else acc` widens to `Item | Null`.
Both would otherwise always trip `RL2313`. The analyzer warns (`RL1206`) when
an effectful statement in a fold body never references the accumulator: the
fold serializes work that a `for` loop would run concurrently.

Named aggregate helpers (`list.sum` and friends) are deliberately absent:
one way to express a reduction. See STDLIB.md.

### 2.7.3 `assert`

`assert(condition[, message])` is an eager statement that checks a runtime
invariant without adding checked data to the program's return value. The
condition must be Boolean and the optional message must be a string. A false
condition raises the non-retryable `ASSERTION_FAILED` error; prior tool effects
are not rolled back.

### 2.7.4 `fail`

`fail(code, message[, details])` raises a catchable error, exactly like a
failing tool call: same `ToolError`, same boundary ownership, span stamped
from the expression. Errors raised by `fail` are never retryable — a
model-detected logic condition does not get better on the next attempt; if
retry semantics are wanted, let the underlying tool fail instead. The
optional `details` object merges into the `err` value a `catch` block
receives. `fail` is an expression of type `Never`, which unifies with
everything, so both positions work: a guard statement
(`g = fail("EMPTY", "no matches") if xs == []`) and an exhaustive alternative
(`first = xs[0] if xs != [] else fail("EMPTY", "expected matches")`).

### 2.8 Error boundaries

An error boundary is structured error handling for the reachable subgraph produced inside its body:

```runlet
report = boundary retry 2 {
    orders = shop.orders(customer.id)
    scores = for order in orders {
        return fraud.score(customer, order, limits)
    }
    return ai.summarize({ customer, orders, tickets, scores })
} catch err {
    return ai.summarize({ customer, tickets, warning: err.message })
}
```

`retry 2` means at most two retries after the initial attempt: three total attempts. Retry applies to retryable failures from reachable nodes created within the boundary, including descendant loop/branch nodes. Dependencies created outside the boundary are not retried. A durably successful call inside a failed attempt is reused only when its complete `operation_id` from section 4.2 is identical: workflow, logical call site, dynamic key, tool schema version, canonical resolved inputs, and operation generation must all match. Reusing a recorded result cannot repeat an effect and is valid for every execution class. A failed or uncertain call is eligible for automatic retry only according to the execution-class table in section 5.4. Distinct duplicate-input loop iterations therefore never reuse one another's result.

Before retry, the runtime cancels unfinished work in the failed attempt and waits for cancellation acknowledgement or a configured cancellation grace timeout. It then applies the host retry policy. The count in source is a cap; the embedder supplies exponential backoff, jitter, maximum duration, and retryable error classification. Source cannot override those safety policies.

The in-process executor implements the backoff part of that policy today: `RuntimeBuilder::retry_backoff(base, factor, cap)` delays re-attempt *k* by `min(cap, base × factorᵏ⁻¹)`, and a retryable `ToolError` carrying `retry_after` (e.g. from an HTTP `Retry-After` header) replaces the computed delay for that attempt, capped by `cap` when backoff is configured. The default is no delay. Backoff is purely a scheduling concern: delays never enter operation identity, canonical values, or the execution graph.

The catch body runs once after attempts are exhausted or immediately for a non-retryable failure. It receives a structured error value:

```text
{
  code: String,
  message: String,
  retryable: Boolean,
  tool: String?,
  node_id: String,
  attempt: Integer,
  details: Object,
  causes: [Error],
  uncertain: Boolean
}
```

`details` is sanitized according to the tool schema and runtime policy. `uncertain` is true when the runtime cannot know whether an externally dispatched operation took effect.

A boundary catches execution failures only: tool failures, timeouts, cancellation failures, conversion failures involving runtime data, and deterministic compute failures such as division by zero. Parse, name-resolution, schema, and other compile errors occur before execution and cannot be caught. Process termination and runtime corruption are resumed by durability, not caught as user errors.

The boundary owns only nodes whose call sites are lexically inside it. It observes failures only from those nodes that are reachable from its returned value. This lexical-ownership plus reachability rule prevents a boundary from accidentally catching failure from a shared upstream dependency.

If a catch body fails, that failure propagates to the enclosing boundary or program. Nested boundaries behave lexically. The error value exists only in the catch scope.

### 2.9 Evaluation of the motivating example

With the required loop `return` added:

1. `crm.customer`, `billing.limits`, and `support.tickets` have resolved `customer_id` inputs, are reachable from `report`, and start concurrently.
2. `shop.orders` is inside the boundary and waits for `customer.id`.
3. When `orders` resolves, the loop expands incrementally. Up to 12 iteration scopes call `fraud.score`; each also depends on `customer` and `limits`.
4. The primary `ai.summarize` waits for the four values in its input object. It does not wait for unrelated graph nodes.
5. A caught failure cancels remaining work owned by the boundary. Eligible retries replay the boundary subgraph without repeating its external dependencies.
6. After retries are exhausted, the catch summary depends on `customer`, `tickets`, and the error, but not `orders`, `limits`, or `scores` except insofar as their failure produced the error.

## 3. Schema-derived opaque types

### 3.1 Schema model

Each registered callable has an immutable descriptor:

```rust
pub struct ToolDescriptor {
    pub name: QualifiedName,
    pub summary: String,
    pub input: CallSchema,
    pub output: Schema,
    pub errors: Vec<ErrorDescriptor>,
    pub execution: ExecutionPolicy,
    pub schema_version: String,
}
```

The portable schema vocabulary is deliberately smaller than full JSON Schema:

```text
Null | Boolean | Integer{min,max} | Number{min,max}
String{format,enum,min_len,max_len} | Bytes
List{items,min_len,max_len} | Object{properties,required,additional}
Map{values} | Union{variants,discriminator?} | Any
```

Properties may include documentation, examples, secret/sensitive marks, deprecation metadata, and aliases used only for diagnostics. Recursive schemas are allowed through registry-local references but MUST have a finite serialization representation. Tools SHOULD prefer discriminated unions.

The registry is frozen before compilation. Its canonical digest, including callable names, normalized schemas, and execution-relevant metadata, is part of the run identity. This makes analysis deterministic and prevents a discovered tool appearing halfway through a run.

### 3.2 Inference and checking

The analyzer performs bidirectional, structural type inference:

- Tool results obtain their type from output schemas.
- Tool arguments provide expected types to literals, objects, and lists.
- Object field projection on a union is allowed only when every variant defines that property; its result schema is the normalized union of the per-variant property schemas. Comparing a discriminant property to a literal in a conditional narrows the selected expression to matching discriminated-union variants. Comparing an optional/nullable expression to `null` with `==` or `!=` narrows the corresponding conditional arm to the null or non-null schema; the opposite arm receives the complementary narrowing. Projection of a partial property is an error with a discriminator-based fix.
- Operators impose built-in shape constraints.
- Conditional branches compute a least safe union; lists infer a common element union.
- Nullability is explicit in schemas even though users do not write it.

An output's hidden future wrapper is orthogonal to its schema type. Diagnostics use source concepts (`customer`, `property`, `tool input`) and never require a user to understand `Output<T>`.

Where a schema contains `Any`, static checking becomes necessarily weaker. Tool authors SHOULD avoid `Any`; UIs and diagnostics mark the loss of precision at its origin.

### 3.3 Implicit conversions

Conversions occur only at a typed context: a tool argument, operator, condition, index, or declared intrinsic parameter. Every conversion has a logical, observable `Convert` node, even when fused internally. Literal-only conversions are evaluated by the analyzer and failure is a compile diagnostic. Conversions involving host inputs or tool outputs evaluate at runtime and failure is attributed to the node; it is catchable exactly when the conversion node's call-site expression is lexically owned by a boundary. Thus conversion failures always have a node or compile span, status, and provenance. Conversions are never used to guess a missing property or callable name.

Version 1 conversions are ranked:

1. exact type;
2. lossless structural adaptation;
3. explicit, deterministic formatting to string;
4. guarded numeric widening or format parsing explicitly enabled by the embedder.

The default conversion matrix is:

| From | To | Default behavior |
|---|---|---|
| Integer | Number | Allowed if exactly representable as binary64; otherwise runtime/compile error |
| Integer, Number, Boolean | String | Canonical scalar formatting defined below |
| Object, Map, List | String | Canonical compact presentation JSON when every transitive leaf is JSON-representable |
| Bytes | String | Not implicit; encoding is ambiguous |
| String | Integer/Number/Boolean/time | Not implicit by default; enable per parameter or use a parsing intrinsic |
| Number | Integer | Not implicit; may truncate or overflow |
| Any value | optional/union containing its type | Allowed |
| Null | optional non-nullable object property | Allowed; the key is omitted from the converted object |
| Object | Object | Allowed when every required target property is present and each property converts |
| List | List | Element-wise when each element converts |

The Null row exists because object literals have a fixed shape: `key: value if condition else null` is the only way to express a conditional property. Converting that null into an optional property that does not itself accept null drops the key, matching how JSON APIs treat null-as-absent. A required property, or an optional property whose schema is nullable, keeps the null and converts (or fails) normally.

Non-finite floats are not valid portable values and cannot be produced by canonical numeric operations. `-0.0` formats as `0`; object keys must be strings. These rules make object-to-string conversion stable across replay and platforms.

Canonical presentation strings are pinned as follows. Integer is optional `-` plus base-10 digits with no leading zero. Boolean is `true` or `false`. Binary64 Number serialization follows RFC 8785 section 3.2.2.3 / ECMAScript `Number::toString`: shortest round-tripping decimal, lowercase `e`, the standard fixed/scientific thresholds, explicit `+` on positive scientific exponents where that algorithm requires it, and zero (including negative zero) as `0`. Runlet language version 1 pins that algorithm; a runtime cannot substitute host `printf`, locale formatting, or a newer incompatible algorithm.

JSON strings use RFC 8785 section 3.2.2.2 exactly: quote and backslash are escaped; U+0008, U+0009, U+000A, U+000C, and U+000D use `\b`, `\t`, `\n`, `\f`, and `\r`; other U+0000–U+001F scalars use lowercase `\u00xx`; `/` is not escaped; and all other Unicode scalar values are emitted as their UTF-8 bytes. Lone surrogates and invalid UTF-8 are rejected before values enter the portable domain. Canonical object/list presentation JSON uses those scalar/string rules, lexicographically sorted UTF-8 key bytes, and no whitespace.

Map presents identically to a JSON object because its keys are strings. Null is valid as a leaf and presents as `null`, even though top-level Null does not implicitly convert to String. Bytes has no implicit JSON representation: an Object/Map/List-to-String conversion whose statically possible transitive leaves include Bytes is a compile error recommending an explicit host-selected encoding intrinsic. If imprecise `Any` data reveals a Bytes leaf only at runtime, conversion fails catchably as `RL5207 NOT_JSON_REPRESENTABLE`. Runlet never guesses base64 versus hex.

Hashing, caching, operation identity, and journal value digests do not hash presentation JSON. They use the versioned Runlet Canonical Value Encoding (RCVE v1), a prefix-free binary encoding:

| Value | RCVE v1 bytes |
|---|---|
| Null | tag `0x00` |
| Boolean | `0x01` false, `0x02` true |
| Integer | tag `0x10`, then minimal signed zig-zag LEB128 |
| Number | tag `0x11`, then 8-byte big-endian IEEE-754 bits; negative zero normalized to positive zero; non-finite forbidden |
| String | tag `0x20`, unsigned minimal LEB128 UTF-8 byte length, then validated UTF-8 bytes |
| Bytes | tag `0x21`, unsigned minimal LEB128 length, then raw bytes |
| List | tag `0x30`, unsigned minimal LEB128 element count, then each RCVE value in order |
| Object/Map | tag `0x31`, unsigned minimal LEB128 entry count, then String-encoded key and RCVE value pairs sorted by raw UTF-8 key bytes |

Minimal LEB128 forbids redundant terminal zero groups. Duplicate object/map keys are invalid before encoding. Schema version is already a separate operation-identity component, so structurally equal Object and Map values intentionally share value bytes. RCVE version is pinned in the run header's value encoding version. The exact RCVE bytes feed all canonical digests; tool wire encodings are executor concerns and do not change identity. Conformance fixtures include boundary values, byte arrays, escaping edge cases, and subnormal floats on every supported architecture.

`null` does not implicitly convert to String. A nullable source passed to a required String is an error offering `value if value != null else "fallback"` or a deliberate formatting intrinsic. This prevents absent data silently becoming the prose text `null`.

Object/list-to-string is convenient but can leak secrets or create unexpectedly large tool inputs. Therefore:

- secret taint propagates through composites and formatting;
- formatted strings inherit the strongest sensitivity label of their inputs;
- each conversion respects configurable byte and depth limits; and
- a compile-time lint suggests an explicit `json.encode` intrinsic where a target parameter is human prose rather than a serialization field.

When more than one union variant would accept an argument at the same conversion rank, compilation fails and lists the competing variants. Runlet never picks based on registry order.

### 3.4 Property and call diagnostics

Unknown names use a weighted candidate search over the relevant scope. The score combines Damerau-Levenshtein distance, common-prefix length, keyboard adjacency, schema aliases, and expected-type compatibility. Candidates outside the current object/namespace are labeled rather than silently substituted.

Example:

```text
RL2103: property `emali` does not exist on `customer`
  --> main.run:7:18
   |
 7 | recipient = customer.emali
   |                      ^^^^^
   |
help: replace `emali` with `email`
closest properties: email, secondary_email, mailing_address
schema: `crm.customer` result (registry version 4b2d…)
```

Every diagnostic contains a stable code, severity, primary span, plain-language explanation, and at least one of:

- a machine-applicable source edit;
- a list of valid candidates or accepted shapes;
- the command/API action that exposes missing schema information; or
- a concrete statement that the host/tool must change, if no source repair exists.

The compiler emits diagnostics as structured JSON as well as rendered text. Fix edits include byte ranges and replacement text and are rejected if their source digest no longer matches.

### 3.5 Tool failures versus language failures

Tool errors preserve the tool's stable error code and structured details. The runtime adds call-site and dependency context without rewriting the cause. A failure view SHOULD show:

1. what failed;
2. whether it is safe/reasonable to retry;
3. the shortest dependency path from program output to the failure;
4. sanitized resolved inputs or which inputs never resolved; and
5. a tool-authored remediation, if supplied.

## 4. Graph semantics

### 4.1 Node kinds

The runtime graph is a directed acyclic graph within each attempt. Logical cycles are impossible without user functions or mutation; the planner still detects internal cycles defensively.

Core node kinds are:

- `Root`: the program return;
- `Call`: a registered tool invocation;
- `Compute`: a deterministic operator or intrinsic;
- `Convert`: a schema-directed conversion;
- `Project`: member or index selection;
- `Composite`: list/object assembly;
- `Branch`: deferred conditional selection and expansion;
- `Loop`: collection resolution, bounded expansion, and ordered collection;
- `Iteration`: structured loop child scope;
- `Boundary`: attempt ownership, retry, and catch selection; and
- `External`: a resolved input supplied by the host.

For performance, trivial projections and composites MAY be fused internally, but the observability layer must reconstruct their logical identities and source spans.

### 4.2 Stable identity

Every logical node has a `NodeId` derived from:

```text
run_id + source_unit_id + syntax_path + lexical_scope_path + dynamic_key + attempt
```

`syntax_path` is a stable path through the parsed syntax tree, based on structural sibling positions and a normalized subtree fingerprint rather than raw byte offsets. `dynamic_key` is empty for static nodes, the input index plus element digest for loop iterations, and the selected arm for branches. `attempt` distinguishes non-reused retries.

Tool dispatch receives two separate identities:

- `operation_id` identifies the semantic external operation and is derived from workflow identity, logical call site, dynamic key, tool schema version, canonical resolved inputs, and an operation generation; and
- `dispatch_id` identifies one transport attempt of that operation and adds a monotonically increasing dispatch generation.

The operation generation remains stable when an idempotent/recoverable operation is redispatched so the tool can deduplicate it. It changes only when policy explicitly authorizes a fresh external operation rather than recovery/retry of the old one. Boundary attempt number alone does not silently change it. Tool authors must treat identical `operation_id` values as the same requested operation when they claim idempotency support. Section 5.4 defines the rules by execution class.

Source edits create a new workflow version. The runtime never guesses that an in-flight old call corresponds to a new source call. Migration requires an explicit host-provided mapping and is deferred beyond version 1.

### 4.3 Edge kinds

Edges are labeled so UIs and schedulers need not infer meaning:

- `data(path)`: a value or projected leaf flows to an input path;
- `control(condition)`: branch selection;
- `contains`: structured ownership by loop/boundary/attempt;
- `orders`: deterministic local ordering, used sparingly;
- `retry_of`: an attempt relationship; and
- `fallback_of`: catch-path relationship.

Data edges record both producer output paths and consumer input paths. Sensitive paths can be visible while their values remain redacted.

### 4.4 State machine

Logical node transitions are normative:

```text
planned -> blocked | pruned
blocked -> ready | pruned
ready -> dispatching | failed | cancelling
dispatching -> running | failed | cancelling
running -> succeeded | failed | cancelling
cancelling -> cancelled
```

`succeeded`, `failed`, `cancelled`, and `pruned` are terminal and have no outgoing transitions. A ready node that loses its boundary/loop scope while waiting for a permit follows `ready -> cancelling -> cancelled`; `pruned` is reserved for work that never became eligible.

`dispatching` is persisted before invoking the tool. A crash in this state may leave an uncertain external effect. On recovery, the runtime consults the tool's recovery policy: query by operation ID, safely redispatch, request operator resolution, or fail with `uncertain: true`. It must not blindly claim exactly-once behavior.

Each transition appends an event before publishing it to observers. Snapshots accelerate recovery but the journal is authoritative.

### 4.5 Scheduling

The scheduler uses dependency counts plus bounded ready queues. Resolving an output decrements its consumers' unresolved counts; a transition to zero makes a node ready. This is O(nodes + edges) for a fully materialized graph.

Concurrency controls compose by taking all applicable permits:

- runtime-wide maximum;
- per-tool maximum;
- tool-declared resource group (for example API tenant);
- boundary attempt cancellation state; and
- loop iteration permit.

Permit acquisition uses a globally ordered resource-key list to avoid deadlocks. Queues use weighted fair scheduling across runs, with optional priorities supplied by the host but not source code. Backpressure stops loop expansion and external result ingestion before memory bounds are exceeded.

Ready independent calls may start in any order. Programs cannot observe scheduling order except through external tools, which must not rely on it. Result list/object order remains deterministic.

### 4.6 Cancellation

Dropping a branch, failing a loop, or retrying a boundary propagates a cancellation token to owned running calls. Tool executors report whether cancellation is supported and whether completion after cancellation is possible. Late results append an `OrphanedCompletion` event referencing the terminal cancelled node; they are evidence, not a node-state transition, and cannot satisfy a newer attempt. If such a result represents an external side effect, it remains visible as an orphaned completion in observability.

Program cancellation is durable. Resumption does not restart a user-cancelled run unless the host explicitly creates a new resume generation.

## 5. Durable execution

### 5.1 Persistence contract

The core runtime depends on a `Journal` interface rather than a database:

```rust
#[async_trait]
pub trait Journal: Send + Sync {
    async fn create_run(&self, header: RunHeader) -> Result<(), JournalError>;
    async fn append(&self, run: RunId, expected_seq: u64, events: &[Event])
        -> Result<u64, AppendError>;
    async fn read(&self, run: RunId, after: u64, limit: usize)
        -> Result<EventPage, JournalError>;
    async fn load_snapshot(&self, run: RunId) -> Result<Option<Snapshot>, JournalError>;
    async fn store_snapshot(&self, snapshot: Snapshot) -> Result<(), JournalError>;
}
```

Append uses optimistic sequence checks. A production adapter must provide atomic append per run and durable bytes before acknowledgement. The first implementation should include in-memory and SQLite adapters; PostgreSQL or a distributed log can follow without changing language semantics.

### 5.2 Event model

Events are versioned, checksummed envelopes using a stable binary encoding (recommended: MessagePack or protobuf with explicit compatibility tests). Representative events:

```text
RunCreated, GraphNodePlanned, GraphEdgeAdded, NodeReady,
CallDispatchPrepared, CallDispatchAcknowledged, CallHeartbeat,
NodeSucceeded, NodeFailed, CancellationRequested, NodeCancelled,
OrphanedCompletion,
BranchSelected, LoopCollectionResolved, IterationMaterialized,
BoundaryAttemptStarted, BoundaryRetryScheduled, CatchSelected,
RunSucceeded, RunFailed, RunCancelled, SnapshotWritten
```

Large values are content-addressed blobs. Journal events store a digest, size, media/schema metadata, sensitivity label, encryption metadata, and optional inline bytes below a small threshold. Blob insertion must complete before the event referencing it commits. Garbage collection retains blobs reachable from non-expired run journals and snapshots.

### 5.3 Replay

Recovery loads the latest compatible snapshot, verifies its journal position and workflow/registry digests, then replays later events. It reconstructs node states and resumes only unfinished effects. Completed tool calls and nondeterministic intrinsics are never re-dispatched merely to rebuild memory; their validated canonical results are retained as journal-referenced values. Cheap deterministic `Compute`, `Convert`, `Project`, and `Composite` results may instead be recorded as `NodeSucceeded { canonical_digest, storage: Recomputable }` and recomputed from retained inputs during recovery. Recomputed bytes MUST match the recorded digest before satisfying consumers; a mismatch fails recovery as `RL7108 DETERMINISM_VIOLATION`. The host configures the cost/size threshold, which is pinned in the run header. Expensive or input-unavailable deterministic results are persisted normally.

Nondeterminism—wall time, randomness, generated IDs, environment reads—may occur only in registered nondeterministic intrinsics or tools, whose results are journaled. Graph planning itself is deterministic given source, registry, external inputs, configuration, and recorded nondeterministic results.

Retry timers store an absolute wall deadline plus originally selected duration. On recovery, an elapsed deadline fires immediately; it is not reset.

### 5.4 Dispatch guarantees

Runlet distinguishes:

- `pure`: deterministic and side-effect-free; safe to recompute and cache by canonical input digest;
- `idempotent`: effects deduplicated by the provided key; safe to redispatch with the same key;
- `recoverable`: executor can query outcome by key before deciding;
- `at_most_once`: runtime will not automatically redispatch after acknowledged dispatch, but outcome may be uncertain after a crash; and
- `unsafe`: no automatic crash recovery or retry; an uncertain dispatch requires catch/operator action.

These are declared and enforced metadata, not inferred promises. The registry loader can prohibit unsafe tools or require an enclosing boundary. A retry request does not override a tool's safety classification.

The normative retry/recovery behavior is:

| Class | Durably successful earlier call | Confirmed retryable failure | Crash/timeout with uncertain outcome | Operation identity |
|---|---|---|---|---|
| `pure` | Reuse validated result | Recompute; no external effect | Recompute | Stable for equivalent inputs; generation never changes automatically |
| `idempotent` | Reuse validated result | Redispatch if policy permits | Redispatch if policy permits | Same `operation_id`, new `dispatch_id` |
| `recoverable` | Reuse validated result | Redispatch if policy permits | Call `recover`; reuse success, redispatch only if outcome is definitely not applied, otherwise surface uncertainty | Same `operation_id` through recovery/redispatch |
| `at_most_once` | Reuse validated result | Retry only if no dispatch was durably prepared; otherwise propagate | Never auto-redispatch; surface uncertainty | Stable across boundary retry; host may authorize a fresh generation only if the previous generation never reached `CallDispatchPrepared` |
| `unsafe` | Reuse validated result within this pinned run | Never auto-retry after dispatch | Never auto-recover; require catch/operator action | Host-authorized fresh generation only |

`CallDispatchPrepared` is the cutoff for the conservative `at_most_once` rule, even if the executor may not actually have received the request. A host can explicitly resolve an uncertain operation through a journaled recovery API. Creating a fresh operation generation is a separate, authorized recovery decision; ordinary boundary retry cannot do it. For `pure` calls, generation remains present in the uniform identity structure but never changes automatically. This separates retries of one semantic operation from intentional repetition of an effect.

### 5.5 Compatibility

A run header pins:

- language version;
- compiler/runtime semantic version;
- normalized source digest and retained source;
- registry digest and retained normalized descriptors;
- intrinsic implementation versions;
- host inputs and configuration digest;
- canonical keyed-hash algorithm plus tenant key identifier/version (secret key material remains outside the journal); and
- value/event encoding versions.

Patch releases may resume a run only if their declared replay compatibility includes its semantic version. Otherwise the old worker must remain available or the run must be explicitly migrated/failed with an actionable operator message.

## 6. Observability API

### 6.1 Snapshot and event stream

The runtime exposes both a consistent graph snapshot and an ordered event subscription:

```rust
pub trait Observer {
    fn snapshot(&self, run: RunId, view: ViewPolicy) -> GraphSnapshot;
    fn subscribe(&self, run: RunId, after_seq: u64, view: ViewPolicy)
        -> Pin<Box<dyn Stream<Item = Result<GraphEvent, ObserveError>> + Send + 'static>>;
}
```

Consumers subscribe from the snapshot's sequence number, eliminating the snapshot/stream race. Slow consumers can reconnect by sequence; the scheduler never waits for UI consumers.

Each public node includes identity, kind, display label, source span, owning scopes, state and timestamps, attempt, tool metadata, input/output schema summaries, retry/cancellation information, and authorized value previews. Edges include source/target value paths.

### 6.2 Security and secrets

Schemas label paths as public, sensitive, or secret. Labels propagate through projections, composites, conversions, compute nodes, error details, logs, cache keys, and blobs. Secret values are never placed directly in journal event metadata or graph labels. A canonical input containing any transitively sensitive or secret path MUST use the run-pinned keyed-hash algorithm/key version; fully public inputs use the run-pinned unkeyed algorithm. This trigger is deterministic from the pinned schemas and propagated labels, not an implementation judgment.

`ViewPolicy` is supplied by the host and authorizes metadata and value paths separately. Redaction happens before events cross the runtime API boundary. Tool implementations may return additional dynamic redaction paths.

Tracing integrates with OpenTelemetry: a run is a trace, structured scopes are spans, and tool attempts are child spans. Trace IDs are stored in journal events. Metrics include ready/running counts, queue latency, tool latency, retry/cancellation counts, journal latency, graph expansion, and blob bytes.

### 6.3 UI behavior

A graph UI can render static source regions before dynamic nodes exist, then add iterations/selected branches as events arrive. It should distinguish:

- blocked on data (with named producer paths);
- blocked on concurrency/resource permits;
- scheduled retry and deadline;
- cancelled versus pruned/not materialized;
- known failure versus uncertain external outcome; and
- reused successful work versus a fresh attempt.

For large loops, the API supports iteration ranges, state aggregation, and sampled/failed-item expansion rather than forcing every node into the browser.

## 7. Host embedding and standard prelude

### 7.1 Runtime construction

The host builds an immutable environment:

```rust
let runtime = Runtime::builder()
    .language(LanguageVersion::V1)
    .registry(registry)
    .intrinsics(default_intrinsics())
    .executor(executor)
    .journal(journal)
    .blob_store(blob_store)
    .limits(limits)
    .build()?;

let compiled = runtime.compile(source, supplied_input_schema)?;
let run = runtime.start(compiled, supplied_inputs).await?;
```

External inputs such as `customer_id` are named resolved values with schemas. Unknown globals are compile errors and suggest host inputs or registered names.

The core separates descriptors from implementations. Compilation can happen in an untrusted front end that possesses schemas but no credentials; execution resolves descriptor IDs against host-owned implementations.

### 7.2 Tool executor

```rust
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    async fn dispatch(
        &self,
        tool: ToolId,
        input: CanonicalValue,
        context: CallContext,
    ) -> Result<ToolOutput, ToolError>;

    async fn recover(
        &self,
        tool: ToolId,
        operation_id: OperationId,
        context: RecoveryContext,
    ) -> Result<RecoveryOutcome, ToolError>;
}
```

`CallContext` includes run/node/attempt IDs, operation and dispatch IDs, deadline, cancellation token, trace context, capability token, and schema version. Tools return values separately from sanitized observability metadata.

The runtime validates fully resolved input immediately before dispatch and validates output before journal success. A malformed output is a non-retryable `TOOL_OUTPUT_SCHEMA_MISMATCH` by default and points to both the tool implementation and affected downstream source.

### 7.3 Minimal prelude

Language operators cover arithmetic, comparison, Boolean logic, member/index access, and list/object construction. Everything else is an intrinsic or tool and can be omitted/replaced by the host.

The recommended deterministic intrinsic namespaces are `text`, `regex`,
`list`, `json`, `number`, and `time` (`hash` is reserved for stable non-secret
digests). There is deliberately no `object` namespace: computed keys, the `+`
merge operator, membership tests, and `for`/`fold` iteration cover its whole
territory in the grammar. Aggregation is not an intrinsic concern: `fold` is the single way to reduce and `skip` the single
way to filter, so the stdlib carries no `sum`/`count`/`filter` helpers. The
complete surface, its inclusion criteria, the no-lambda design, operator
overloading decisions, and the regex dialect choice are specified in
[`STDLIB.md`](STDLIB.md).

`now()` and randomness are nondeterministic intrinsics whose first results are journaled. They are optional. The current time is not an implicit global.

`buffer()` from the discovery illustration is not a language feature. A host may register a deterministic `buffer` builder, but mutation such as `out.write(...)` conflicts with immutable dataflow unless modeled as a chain of values. The portable equivalent is:

```runlet
time = now()
github_tools = tool_search("github")
linear_tools = tool_search("linear")

rows = for tool in github_tools + linear_tools {
    return { name: tool.name, arguments: tool.arguments, return_type: tool.return_type }
}

return { time, tools: rows }
```

If an embedder exposes a buffer handle, each `write` must return a new handle or be an explicitly ordered effect whose token is returned. Such host extensions are outside portable Runlet and must declare durability behavior.

### 7.4 Progressive tool disclosure

The discovery flow is two separate runs:

1. Run A is compiled against the base registry containing `tool_search`.
2. It returns tool descriptors as ordinary data.
3. The host validates policy/authorization, converts selected descriptors into registry entries backed by implementations, and freezes a new registry.
4. Run B is compiled and executed against that registry.

Run A cannot invoke names it discovers. Run B diagnostics can state which registry digest was searched. This separation prevents runtime namespace mutation, schema races, and confused-deputy authorization failures.

## 8. Compiler architecture in Rust

### 8.1 Crate layout

Recommended workspace:

```text
crates/
  runlet-syntax       lexer, lossless CST, parser, formatter
  runlet-diagnostics  spans, codes, rendering, machine fixes
  runlet-schema       portable schemas, normalization, compatibility
  runlet-hir          resolved names and desugared high-level IR
  runlet-analyzer     inference, conversions, reachability, lints
  runlet-plan         typed graph templates and stable identities
  runlet-value        canonical values, blobs, redaction/taint
  runlet-runtime      expansion, state machines, scheduler, recovery
  runlet-journal      persistence traits and in-memory adapter
  runlet-journal-sqlite
  runlet-observe      snapshots, events, OpenTelemetry bridge
  runlet-sdk          stable embedding facade
  runlet-cli          check, format, explain, run, graph, replay
```

Keep parsing/schema/value crates independent of Tokio. The runtime may initially use Tokio but exposes cancellation, clocks, persistence, and spawning behind narrow interfaces where practical. Public serialized types live in versioned crates and do not expose internal arena indices.

### 8.2 Compilation pipeline

1. Lex into tokens retaining trivia and byte offsets.
2. Parse into an error-tolerant, lossless concrete syntax tree.
3. Lower to AST/HIR, creating stable syntax paths and lexical scopes.
4. Resolve globals against inputs, intrinsics, and the frozen registry.
5. Infer structural types and future lifting; insert conversion nodes.
6. Check effects, boundary ownership, loop limits, reachability, and host policy.
7. Lower static regions to graph templates; retain dynamic branch/loop templates.
8. Compute normalized source, registry, plan, and compatibility digests.
9. Emit a `CompiledProgram` with source map and schema provenance.

Compilation has no network access and invokes no tools. It should be deterministic and safe for untrusted source, with limits on source bytes, nesting, literal sizes, union expansion, diagnostic count, and analysis time.

### 8.3 Parser strategy

Use a hand-written lexer plus Pratt parser for expressions and recursive descent for statements. This gives precise recovery and contextual diagnostics with a small grammar. The lossless CST enables formatting and exact machine edits; HIR removes shorthand and distinguishes registry paths from projections.

Error recovery synchronizes at newlines, semicolons, and closing braces while tracking delimiter balance. The parser should prefer one local repair (insert/delete/replace token) and continue, avoiding cascades. Fuzz with arbitrary UTF-8 and assert no panic, bounded memory, and stable diagnostics.

### 8.4 Analyzer representation

Use interned strings and schemas, arena-allocated HIR, compact integer IDs, and immutable shared schema nodes (`Arc` or a schema arena). Represent types as normalized unions with a configured maximum variant count; widen excessive unions to `Any` with a precision-loss diagnostic rather than exponential behavior.

Analysis values carry:

```rust
struct ExprInfo {
    schema: SchemaId,
    readiness: Readiness, // Resolved or Output
    sensitivity: Sensitivity,
    origin: Origin,
    plan_ref: PlanValueRef,
}
```

Object values should preserve source insertion order for diagnostics while canonical serialization sorts keys. Dependency leaves use compact path tries so a composite does not require a quadratic edge set.

### 8.5 Runtime representation

Store nodes in generational arenas and refer to them by compact indices internally. Separate immutable plan metadata from mutable execution state. Hot scheduler state should avoid a single global lock: shard runnable queues and per-run state, use atomics for dependency counts, and serialize journal mutations per run through a lightweight actor/task.

Do not publish a node as runnable until its planning/edge events are durably appended. Batch adjacent graph and transition events within small byte/time bounds. Never hold scheduler locks across journal or tool awaits.

Canonical values should borrow/share large strings and byte blobs, spill above thresholds, and avoid repeated JSON reserialization. Canonical hashes can be computed incrementally during validation. Cache only according to sensitivity, tenant isolation, and tool policy.

## 9. Error design

### 9.1 Taxonomy

Stable top-level categories:

- `RL1xxx` syntax and structure;
- `RL2xxx` name, schema, type, and conversion;
- `RL3xxx` effect, reachability, safety, and policy;
- `RL4xxx` planning and resource limits;
- `RL5xxx` runtime compute/data failures;
- `RL6xxx` tool execution and recovery;
- `RL7xxx` persistence/replay/compatibility; and
- `RL8xxx` host integration errors.

Codes are never reused. Rendered wording can improve without breaking programmatic consumers.

### 9.2 Actionability contract

An error is actionable when a model can choose the next operation without guessing. Each error record contains:

```text
code, title, phase, severity,
primary_span, secondary_spans[],
dependency_path[], schema_origin?, tool?,
actual_shape?, expected_shapes[], candidates[],
fixes[], retry/recovery metadata?, documentation_key
```

Examples:

```text
RL2208: `linear.issues` requires property `owner`
  --> main.run:4:27
help: add `owner: linear_me.id`
accepted input: { owner: String, state?: String, limit?: Integer }
```

```text
RL2311: cannot safely convert `4.7` from Number to Integer for `limit`
help: use `number.floor(4.7)`, `number.ceil(4.7)`, or `number.round(4.7)`
```

```text
RL6109: outcome of `payments.charge` is unknown after worker loss
node: charge in checkout.run:18:9
help: query the provider with operation ID `…`, then resolve this node through the host recovery API
```

The CLI offers `runlet explain RL2103` from bundled documentation, so resolving an error never requires network access.

### 9.3 Multiple failures

Independent parallel failures may race. The first non-cancellation failure durably committed for an attempt triggers cancellation and becomes the primary error. Other non-cancellation failures observed before owned work reaches terminal state or the cancellation grace deadline appear in `causes`, ordered by node identity. Cancellation-induced errors are never candidates. This choice is replay-stable for a fixed journal but honestly may vary across fresh executions because external completion order is observable; Runlet does not pretend it can know failures that cancellation prevented. A UI exposes the trigger sequence number and all observed causes.

## 10. Security and resource governance

Runlet source is untrusted input. The host controls all authority; a registry entry is both a schema and a capability. Merely spelling an unavailable tool cannot acquire it.

Required configurable limits include source size, parse nesting, object/list literal size, graph nodes/edges, dynamic iterations, loop concurrency, total calls, call input/output bytes, canonical-conversion depth/bytes, wall time, retries, journal/blob bytes, and diagnostic work. Limit failures identify the limit, current amount, configured cap, responsible source span, and host configuration key.

Tools receive scoped capability tokens rather than ambient host credentials when possible. Registry search results must be policy-filtered before reaching the agent. Descriptor text is untrusted data and never executed as source automatically.

The runtime should support per-tenant encryption keys, journal/blob retention, audit events, egress controls in the executor, and sensitive-value zeroization where practical. Rust memory safety does not remove the need to avoid values in panic messages and telemetry.

## 11. Formal invariants

An implementation is conforming only if these hold:

1. **Dependency safety:** a node is dispatched only after all required data/control dependencies succeed and its input validates.
2. **Reachability safety:** an effectful node not transitively reachable from the selected root is never dispatched.
3. **Single selection:** at most one arm of a dynamic conditional is materialized.
4. **Loop bound:** no more than the host-configured number of iteration scopes of a loop are active.
5. **Boundary ownership:** retry/catch/cancellation affects only reachable lexically owned descendants.
6. **Replay stability:** the same pinned source, registry, inputs, configuration, and journal prefix reconstruct the same logical graph and state.
7. **Durable transition:** no externally observable state transition is published before its journal event commits.
8. **No false exactly-once claim:** uncertain external effects remain explicitly uncertain until an executor/host resolves them.
9. **Deterministic aggregate:** list/object result ordering does not depend on completion order; primary-error selection is stable for replay of a fixed journal and follows the explicit first-committed rule across fresh runs.
10. **Redaction closure:** derived values cannot have lower sensitivity than any contributing input without an explicit trusted declassification operation.
11. **Diagnostic repair:** every fatal error identifies a source repair or a named host/tool/operator action.
12. **Bounded planning:** configured source and graph limits prevent unbounded memory/CPU use from a single run.

## 12. Testing and validation strategy

### 12.1 Language tests

- Golden lexer/parser/HIR/diagnostic tests, including malformed agent-generated code.
- A mechanical grammar test that every referenced EBNF nonterminal is defined exactly once, plus recovery fixtures for statement-form control and early/duplicate returns.
- Lexical fixtures for every reserved word as a binding, field, and object key, plus newline boundaries before `(`, `[`, and infix operators.
- Schema inference and conversion matrix tests across unions, optionals, and composites.
- Operator-by-schema fixtures covering checked overflow, division/remainder, structural equality/membership, short-circuiting, list unions, and rejected relational/equality/mixed chains.
- Cross-platform RCVE/presentation JSON byte-and-digest fixtures for integers, Bytes containing every octet, `0.1`, `1e21`, `-0.0`, the smallest subnormal binary64, every JSON escape case, and nested objects with non-ASCII keys.
- Presentation-conversion fixtures for nested Map/Null and statically/dynamically discovered Bytes leaves, including `RL5207`.
- Call-normalization fixtures proving `f(a, b)` and `f({ x: a })` retain distinct canonical argument-list envelopes and operation digests.
- Index/projection fixtures for positive/negative/boundary indices over list/String/Bytes, literal and dynamic object/map keys, `Any` dispatch, and stable `RL5203`/`RL5205`/`RL5206` failures.
- Registry-root collision and discriminated-union narrowing/projection fixtures.
- Snapshot tests ensuring diagnostics contain valid, non-overlapping machine edits.
- Formatter idempotence and parse-format-parse equivalence.
- Property/fuzz tests for parser non-panics and bounded behavior.

### 12.2 Runtime model tests

Build a deterministic simulated executor, clock, journal, and scheduler. Generate small programs/graphs and explore completion, failure, cancellation, and crash interleavings. Check all formal invariants after every transition. Particularly test:

- crash before/after dispatch preparation and acknowledgement;
- simultaneous failures, first-committed primary selection, and fixed-journal replay stability;
- nested boundary retry ownership;
- execution-class retry tables, asserting operation/dispatch IDs and dispatch counts for success, confirmed failure, uncertain outcome, and duplicate-input loop iterations;
- loop permit release during failure/cancellation;
- late completion from cancelled attempts;
- snapshot plus event subscription without gaps;
- secret propagation through canonical string conversion;
- catchable runtime conversions from host inputs versus compile-time literal conversion diagnostics;
- million-item loops with bounded materialization.

Use `loom` for targeted Rust concurrency structures and proptest/state-machine testing for scheduler semantics. Fuzz event decoding and replay with truncation, duplication, checksum errors, and unknown future event variants.

### 12.3 Conformance suite

Publish versioned fixtures containing source, registry schemas, host inputs, expected diagnostics or normalized plan, executor transcript, journal events, final value/error, and redacted graph view. Alternative executors/journals must pass the same fixtures.

### 12.4 Performance targets

Targets must be measured on named hardware and are not semantic promises. Initial engineering goals on a contemporary developer laptop:

- parse and analyze a 10 KiB program with 1,000 registry tools in under 10 ms warm;
- maintain at least 100,000 materialized lightweight graph nodes per process with under 1 KiB runtime overhead per simple node excluding values/source/schema;
- schedule dependency-ready local no-op calls at over 100,000 transitions/second using an in-memory journal;
- keep loop live-node count O(concurrency + prefetch), independent of total input length; and
- add under 1 ms scheduler overhead to ordinary remote tool calls at the p50.

Benchmarks must report journal durability mode; an fsync-backed path is expected to be storage-bound. Optimize only after profiles preserve the invariants and diagnostics.

## 13. Implementation roadmap

### Phase 0: semantic executable model

- Finalize grammar and normalized schemas.
- Implement parser, analyzer skeleton, canonical values, and structured diagnostics.
- Build a single-threaded deterministic graph simulator.
- Encode the motivating examples and invariant/property tests.

Exit: every language construct has executable semantics and golden plans; no real tools or durability.

### Phase 1: useful embedded runtime

- Implement immutable registry, tool executor, Tokio scheduler, boundaries, bounded loops, and observability snapshots/events.
- Add in-memory journal, CLI, default intrinsics, and OpenTelemetry.
- Validate inputs/outputs and sensitivity propagation.

Exit: agent harness can compile/run programs and render live graphs; process restart is not yet promised.

### Phase 2: durable local runtime

- Add SQLite journal/blob store, recovery state machine, snapshots, idempotency/recovery policies, durable timers, and crash-injection suite.
- Stabilize event and compiled-plan encodings.

Exit: kill/restart testing passes across dispatch/retry/cancel interleavings with no false success or duplicate dispatch outside declared policy.

### Phase 3: production hardening

- Fair multi-run scheduling, quotas, tenant isolation, retention/GC, policy hooks, large-graph UI pagination, and compatibility tooling.
- Add a PostgreSQL journal reference adapter if deployment needs it.
- Publish conformance suite and SDK stability policy.

Exit: security review, fault-injection soak tests, documented SLOs, and versioned compatibility guarantees.

## 14. Decisions and deliberately deferred questions

Decided for version 1:

- Pure work is lazy and root-reachable; statements containing effectful calls are implicit roots, so bound fire-and-forget writes always run.
- Loop bodies explicitly `return` or `skip`; host concurrency bounds whole iterations.
- `for` pins bounded concurrency, `fold` pins sequential reduction, `boundary` pins retry: lambdas exist only at these controlled application sites and are never values.
- `retry N` means N retries after the first attempt.
- Conditions have no truthiness.
- Safe object/list-to-string uses canonical JSON and propagates sensitivity.
- Dynamic discovery creates the next frozen runtime; it does not mutate the current registry.
- Exactly-once is not promised for arbitrary tools.
- In-flight source/schema migration is explicit, not heuristic.
- The portable language has immutable bindings and values.

Deferred beyond version 1:

- User-defined pure functions and reusable local subgraphs.
- Streaming collections and backpressured stream transforms.
- A structured `emit` statement for returned-independent durable effects.
- First-class callable tool handles.
- Source-level timeout/backoff syntax (host policy is sufficient initially).
- Explicit host-approved declassification.
- Distributed ownership/leases and cross-process scheduling.

## Appendix A: canonical motivating program

```runlet
customer = crm.customer(customer_id)
limits = billing.limits(customer_id)
tickets = support.tickets(customer_id)

report = boundary retry 2 {
    orders = shop.orders(customer.id)

    scores = for order in orders {
        return fraud.score(customer, order, limits)
    }

    return ai.summarize({
        customer,
        orders,
        tickets,
        scores
    })
} catch err {
    return ai.summarize({
        customer,
        tickets,
        warning: err.message
    })
}

return report
```

## Appendix B: unresolved-output example

```runlet
me = github.me()
linear_me = linear.me()

my_issues = linear.issues({ owner: linear_me.id })
my_prs = github.prs({ owner: me.login })

return my_issues + my_prs
```

The identity calls run concurrently. Each query waits only for its matching identity. `+` becomes a deterministic list-concatenation compute node and waits for both lists. The program root waits for that node.

## Appendix C: terminology

- **Value:** a resolved portable datum.
- **Output:** an unresolved or resolved hidden future datum; not source syntax.
- **Tool:** a host-registered callable that may perform effects.
- **Intrinsic:** a host-registered callable implemented as a local deterministic or journaled operation.
- **Plan/template:** compiler output for graph structure known before runtime values resolve.
- **Materialize:** instantiate runtime nodes from a static or dynamic template.
- **Scope:** structured ownership region such as a loop iteration or boundary attempt.
- **Attempt:** one execution of a boundary body.
- **Journal:** authoritative ordered durable event log for a run.
- **Canonical value:** deterministic portable representation used for validation, hashing, persistence, and conversion.
