# Runlet standard library design

Status: accepted surface, implementation in progress. This document records
the design review of the standard library and the concrete surface it
produced. It extends the sketch in [`DESIGN.md`](DESIGN.md) section 7.3 with
evidence from the AgentKit compose-bench integration, where the prelude — not
the grammar — turned out to be the last gap between Runlet and a mature
scripting runtime on complex agent tasks. The review's yardstick: minimal new
vocabulary, maximum flexibility and capability, never converging on any one
benchmark.

## 1. Principles

The grammar stays closed. Bindings, expressions, objects (including computed
keys), lists, `for`, `fold`, `skip`, `assert`, `fail`, postfix conditionals,
`boundary`, and `return` are the whole language; every capability below is an
intrinsic that shares tool-call syntax, schema checking, and the execution
graph. A model that has learned to call one tool has learned the entire
stdlib.

**Inclusion bar.** A function earns a place when all of these hold:

1. Models reach for the capability unprompted. (Evidence: bench transcripts
   show hand-rolled implementations — a 12-step `split`/`join` chain for
   punctuation stripping, `[x] if keep else []` + `flatten` for filtering,
   literal `[1, 2, ..., 10]` lists for pagination.)
2. Expressing it with loops and conditionals costs a multi-node ceremony that
   obscures intent or invites logic bugs.
3. The signature is schema-checkable and the behavior is total and
   deterministic over canonical values.

**Exclusion bar.** A function is rejected when it duplicates an operator or
another function (one way to do things), when a short loop, fold, or regex
expresses it clearly, or when it would import host state (wall time,
randomness, environment) into the pure tier. This bar is applied to the
library's own entries as ruthlessly as to candidates: several functions from
the first draft were cut for failing it (section 6).

**Uniform failure contract.** Every intrinsic failure is a catchable
`ToolError` with a span stamped by the evaluator, a message that names the
actual value kind, and a concrete rewrite hint — the pattern established for
`RL5202`/`RL5208`. Intrinsics never return sentinel values on error; absence
is expressed with `null`, failure with an error.

**Naming.** Noun namespaces, verb or predicate functions, Python/JS-familiar
vocabulary, no abbreviations. Model familiarity is a design input: models
have seen `trim`, `starts_with`, `sort_by`, and `test` millions of times.

**Descriptors are documentation.** Each intrinsic's `ToolDescriptor.summary`
is written for the model, not the host. Embedders (like the AgentKit compose
backend) should render the intrinsics section of their primer *from the
registry* instead of maintaining a hand-written list that drifts.

## 2. The no-lambda problem

Runlet deliberately has no function values, so `list.filter(xs, fn)` is
impossible. Three options were considered:

| Option | Verdict |
| --- | --- |
| Grammar lambdas (`x => x.total > 10`) | Rejected: grows the grammar, reintroduces closures/recursion questions. |
| Expression strings (`list.filter(xs, "item.total > 10")`) | Rejected: a second language inside the language, invisible to the analyzer. |
| Blocks at controlled application sites + computed keys | **Adopted.** |

Runlet's loops *are* lambdas — applied only at sites that pin execution
semantics: `for` (host-bounded concurrency), `fold` (sequential reduction),
`boundary` (retry). Four mechanisms close the gap without function values:

**`skip` filters.** `skip if cond` inside a `for` body drops the element;
inside a `fold` body it passes the accumulator through unchanged. Filtering
is fused into iteration, and totality is preserved: every body path must
`return` or `skip`, so a forgotten return is still a compile error rather
than a silently shorter list.

**`fold` reduces.** `fold acc = 0 for x in xs { return acc + x }` is the
single way to aggregate — there are deliberately no `list.sum`/`count`/`min`
helpers duplicating it (one way to express a reduction). It also unlocks what
no intrinsic table could: ordered effect chains and cursor pagination, where
each iteration depends on the previous result. The analyzer warns (`RL1206`)
when a fold's effects never touch the accumulator — that work belongs in a
concurrent `for`.

**Computed keys accumulate by key.** `{ [expr]: value }` (DESIGN.md 2.2)
plus the `+` merge operator make `fold` express every keyed reduction —
grouping, indexing, counting, dedupe-by-key — so the library needs no
`group_by`/`index_by`/`from_entries` vocabulary at all:

```runlet
by_id = fold acc = {} for u in users { return acc + { [u.id]: u } }

groups = fold acc = {} for c in contacts {
    return acc + { [c.company]: (acc[c.company] if c.company in acc else []) + [c] }
}
```

The membership guard is the default idiom: the postfix conditional is lazy,
so the missing-key projection never evaluates. (A `??` null-coalescing
operator was considered as sugar for this guard and deferred: it is *not* a
truthiness violation — it tests null-ness only — but the guard already
expresses the default, and `??` carries a real precedence trap
(`acc[k] ?? [] + [c]` silently parses as `acc[k] ?? ([] + [c])`). Revisit on
transcript evidence.)

**`fail` guards.** `fail(code, message[, details])` raises a catchable,
non-retryable error (type `Never`), completing `boundary`: programs can now
originate what they can catch.

**Key paths** survive only where an intrinsic genuinely needs a projection
it applies internally: `list.sort_by(orders, "customer.tier")`. Because the
item schema is statically known, the analyzer verifies the path at compile
time — something callbacks could never give us.

## 3. Accepted surface — 28 functions, 6 namespaces

Tier 1 is the default prelude. Anything not listed was considered and
excluded (section 6).

### text — 10 functions

| Function | Notes |
| --- | --- |
| `text.length(s)` | Characters (Unicode scalar values), not bytes. Existing. |
| `text.lower(s)` / `text.upper(s)` | Existing. |
| `text.trim(s)` | Both ends, Unicode whitespace. |
| `text.starts_with(s, prefix)` / `text.ends_with(s, suffix)` | Kept despite `^`/`$`: dynamic prefixes from data make the regex form an injection bug. |
| `text.slice(s, start, end)` | Character indices, negative from end, clamped — mirrors list indexing. |
| `text.split(s, separator)` | Existing; non-empty separator. Literal counterpart of `regex.split`. |
| `text.join(strings, separator)` | Existing. |
| `text.replace(s, from, to)` | Existing; literal, all occurrences. Literal counterpart of `regex.replace` — not a duplicate: dynamic `from` strings make patterns unsafe. |

`text.contains` is cut: `needle in s` is the operator form and the exclusion
bar forbids function aliases of operators. The unknown-name diagnostic should
hint the rewrite.

### regex — 5 functions

Regexes are the single highest-leverage addition: models write them
fluently, and one pattern replaces entire normalization subprograms. The
bench's phone task becomes two lines:

```runlet
digits = regex.replace(contact.phone, "[^0-9]", "")
e164 = ("+1" + digits) if text.length(digits) == 10 else ("+" + digits)
```

| Function | Returns |
| --- | --- |
| `regex.test(s, pattern)` | Boolean. Kept even though `captures != null` covers it: validation is the dominant use, and `is_match` avoids capture allocation. |
| `regex.find_all(s, pattern)` | List of full matched strings (never Python's first-group `findall` wart). |
| `regex.captures(s, pattern)` | `null`, or `{ full: String, groups: List, names: Map }` — `groups` are the numbered captures (`null` for unmatched optionals), `names` the named ones. The extraction workhorse. |
| `regex.replace(s, pattern, replacement)` | String; `$1`/`$name` references in the replacement. |
| `regex.split(s, pattern)` | List of strings. |

`regex.find` is cut: it is `regex.captures(s, p).full` behind a null guard —
sugar, and the one-way rule wins.

**Dialect.** The Rust `regex` crate: guaranteed linear-time matching, no
catastrophic backtracking — the right property for a sandbox executing
model-authored patterns. The cost is no lookaround and no backreferences.
Models *will* write `(?=...)`; the compile error must catch it with a hint
("lookahead is not supported; match the prefix and use `regex.captures` to
extract the part you need"). The alternative (`fancy-regex`, full
backtracking) was rejected: model familiarity is not worth unbounded runtime
in a concurrency-limited executor.

**Static validation.** When the pattern is a string literal — the
overwhelmingly common case — the analyzer compiles it at compile time and
reports invalid patterns as span-annotated diagnostics before anything runs.
Dynamic patterns compile at runtime (catchable, size-capped, cached per run).

### list — 5 functions

Everything a `fold` or a `for` body expresses clearly is deliberately absent
(section 6). What remains is what a fold cannot express well:

| Function | Notes |
| --- | --- |
| `list.length(xs)` | A projection, not a reduction — counting via fold is aggregation ceremony for a basic query, and `list.range(0, list.length(xs))` is what makes indexed iteration (zip, enumerate, chunk) expressible at all. |
| `list.sort(xs)` | Natural order for homogeneous scalars; mixed kinds fail catchably (matches no-truthiness strictness; Starlark's total order across kinds silently "works" on garbage). A fold could only express an insertion sort. |
| `list.sort_by(xs, path)` | Dotted key path; optional `"desc"` third argument; `null` values sort last, stable. The `"desc"` string is the one stringly-typed flag in the library — every alternative (a `reverse` function, key negation) is worse. |
| `list.slice(xs, start, end)` | Negative indices, clamped — same rules as indexing. |
| `list.range(start, end)` | Inclusive-exclusive integer range; replaces literal `[1, 2, ..., 10]` pagination lists and gives `fold` an index sequence. Capped by the host node budget. |

`list.group_by` and `list.index_by` from the first draft are cut: computed
keys made them fold-expressible (section 2), and the same rule that removed
`sum`/`count`/`unique` then applies. The primitive is strictly more capable
than the two functions were — counting and summing by key were never
expressible with `group_by` alone.

### json — 2 functions

| Function | Notes |
| --- | --- |
| `json.parse(s)` | String → `Any`. MCP and HTTP tools constantly return stringified JSON; without this the value is opaque. Failure is catchable and names the position. |
| `json.encode(v)` | Canonical compact JSON (RFC 8785 rules already specified for string conversion). Named `encode` (not `format`) after `JSON.stringify`/`json.dumps` intuition. |

### number — 4 functions

| Function | Notes |
| --- | --- |
| `number.round(x)` / `number.floor(x)` / `number.ceil(x)` | Existing. Not expressible otherwise. |
| `number.parse(s)` | String → Integer or Number; the "parsing intrinsic" the conversion matrix defers to. Catchable failure. |

`abs` and `clamp` are cut by the same rule that omits scalar `min`/`max`:
`x if x >= 0 else 0 - x` is the single way. The four stand or fall together.

### time — 2 functions (pure)

Times are **Integer epoch milliseconds** — no new value kind, ordinary
arithmetic and comparisons work, and canonical hashing is untouched.

| Function | Notes |
| --- | --- |
| `time.parse(s)` | ISO 8601 / RFC 3339 → epoch ms. Catchable failure. |
| `time.format(ms)` | Epoch ms → RFC 3339 UTC string. |

`time.add`/`time.diff` from the first draft are cut: epoch milliseconds were
chosen *so that* `start + 3 * 86400000` is ordinary arithmetic — a single
expression, not ceremony — and neither function handled the case where naive
arithmetic actually fails (calendar months). Cutting them also removes the
`"seconds"/"days"` unit-string enum. Tier-2 candidates on transcript
evidence.

`time.now()` is **not** in the pure tier. Wall time is host state; per
`DESIGN.md` section 14 it must be a journaled nondeterministic intrinsic the
host opts into, or (better, and sufficient for every bench scenario) a host
input: `.input("now", Schema::INTEGER, now_ms)`.

### object — no namespace

Cut entirely; the review's centerpiece. Every candidate is existing
vocabulary:

| First draft | Covered by |
| --- | --- |
| `object.get(o, key, default)` | `o[key] if key in o else default` (lazy conditional) |
| `object.merge(a, b)` | the `+` operator (§4 rule: no function aliases of operators) |
| `object.keys` / `object.values` | `for p in obj { return p.key }` — the same one-liner rule that cut `pluck` |
| `object.entry` / `from_entries` / `group_by` / `index_by` | computed keys + `+` merge in a `fold` (section 2) |

### hash — 1 function (tier 2)

`hash.sha256(s)` → lowercase hex. Stable non-secret digests for dedupe keys
and idempotency tokens. Tier 2 because no observed program needed it yet.

Totals: **28 tier-1 functions across 6 namespaces**, plus the grammar
constructs (`skip`, `fold`, `fail`, computed keys) and the `+` merge
operator. Every namespace that parses calls it `parse`, all catchable
(`json.parse`, `number.parse`, `time.parse`). The primer section listing the
library stays well under 500 tokens.

## 4. Operators over functions

Where an operator already carries the intuition, the stdlib must not add a
function alias. Current overloads: numeric `+ - * / %`, `+` as concatenation
(string + anything formattable, list + list) **and shallow right-biased
object merge** (implemented; DESIGN.md 2.4.1), comparisons on numbers and
strings, `in` for list membership, substring, and object keys.

Considered and rejected:

- `string * n` / `list * n` repetition — rare in agent programs, and `*`
  between mixed kinds is a classic source of confusion.
- `list - list` / `object - keys` difference — too clever; a loop with
  `skip` expresses intent more legibly.
- truthy `or` defaults — violates the no-truthiness rule the runtime
  teaches in its error messages. `??` (null-only coalescing) is *not* a
  truthiness violation and remains a deferred candidate — see section 2.
- Deep merge — recursion depth and list-vs-replace semantics are
  unresolvable by intuition; shallow merge plus explicit nesting is
  predictable.

## 5. Nondeterministic intrinsics (opt-in, journaled)

`time.now()`, `random.uuid()`, `random.int(lo, hi)` exist as a separate
tier the host registers explicitly. They are non-`Pure` (so implicit effect
roots apply), their first results are journaled and replayed by operation
id — replay-stable by construction. Default off in the AgentKit compose
backend; a host input is almost always the better design.

## 6. Considered and excluded

Cut from the first draft by the exclusion bar (see sections above for the
individual arguments): `text.contains`, `regex.find`, `list.group_by`,
`list.index_by`, the entire `object` namespace, `number.abs`,
`number.clamp`, `time.add`, `time.diff`.

Never admitted:

- `text.digits(s)` and friends — `regex.replace(s, "[^0-9]", "")` is one
  call; special-casing every character class is a treadmill.
- `list.filter` / `list.map` with any callback mechanism — section 2
  (`for` + `skip` is the one way).
- `list.sum` / `count` / `min` / `max` / `unique` / `flatten` / `compact` /
  `pluck` / `reverse` — all are short folds or loops; duplicating them would
  create a second way to express every reduction.
- `list.zip`, `list.chunk`, `list.window` — `list.range(0, list.length(xs))`
  plus indexing expresses the rare case.
- Scalar `min(a, b)`/`max(a, b)` — `a if a < b else b` is the single way,
  and the list forms are folds.
- `text.format` templates — `+` concatenation covers agent-scale formatting;
  a template mini-language is a second grammar.
- `math.pow/sqrt/log` — not agent-task shaped; add on evidence.
- `object.pick/omit` — null-valued optional properties already vanish at
  call boundaries, which covers the payload-shaping case that motivated
  them.
- Locale-aware casing/collation, base64/bytes codecs, URL parsing — real but
  host-tool territory; they drag in tables/policy the portable core should
  not pin. A host can register `codec.*`/`url.*` tools if its domain needs
  them.

## 7. Delivery and versioning

- `Runtime::builder().with_prelude()` keeps working and now means "tier 1".
  `with_stdlib(Stdlib::tier1().with(Namespace::Hash))` selects namespaces
  explicitly; hosts remain free to register any subset or replacements.
- Every stdlib descriptor carries `schema_version: "stdlib/1"`. The registry
  digest therefore pins the stdlib version into compiled programs and
  operation identity — a behavior change to any intrinsic is a new stdlib
  version, never a silent redefinition.
- Prerequisite: `list.range`, `list.slice`, `text.slice`, and
  `list.sort_by` want optional trailing parameters. `CallSchema` gains
  optional trailing positional parameters (arity range instead of fixed
  arity); the analyzer already has everything needed to check them.

## 8. Open questions

1. **Key-path checking strictness** (now only `list.sort_by`): compile error
   exactly when the item schema is concrete, defer when `Any`/union — same
   rule as member access today. Recommended, pending implementation.
2. **Where tier 2 lives** — same crate behind a builder flag (recommended)
   or a separate `runlet-stdlib-extras` crate.
3. **`??` null-coalescing** — deferred; adopt only on transcript evidence
   that the `key in obj` guard idiom is a real friction point, and decide
   parenthesization rules against the precedence trap then (section 2).
