# How do LLMs even work?

Large Language Models (LLMs) are probabilistic models, typically based on the [transformer architecture](https://arxiv.org/abs/1706.03762), trained via gradient-based machine learning to predict the next token in a sequence.

They don't "think" or maintain persistent memory. During inference, a pre-trained model processes an input sequence and generates output token-by-token. LLMs are stateless across requests, but fully condition on all tokens within the current context window.

Key parameters and concepts:

- **context window size**: the maximum number of tokens (input + output) the model can process in a single request. Frontier models can reach ~1M tokens; ~100k–250k is typical for strong models.
- **temperature**: controls randomness in token sampling. Lower values bias toward high-probability tokens (more deterministic); higher values increase diversity by allowing lower-probability tokens to be selected.
- **weights and fine-tuning**: the model consists of learned parameters ("weights") arranged in matrices across layers. These encode statistical relationships between tokens. Fine-tuning adjusts these weights to specialise behaviour on specific data or tasks.

## Tokenisation

LLMs operate on tokens, not raw text. Tokens are subword units (e.g. "un", "likely", "##hood").

```text
raw text:      "unlikely"
                   │
            ┌──────┴─────┐
tokens:    [un]  [like] [ly]
            │      │      │
token ids: [348] [2193] [306]
```

Implications:

- cost scales with token count, not characters
- prompt design must consider token efficiency
- edge cases (code, JSON, whitespace) matter

---

## Next-token prediction

At its core, an LLM repeatedly does:

1. Encode input tokens into vectors (embeddings)
2. Pass them through transformer layers (attention + MLPs)
3. Produce logits (unnormalised probabilities over vocabulary)
4. Sample/select the next token
5. Append token and repeat

```text
input: [The] [cat] [sat]
         │     │     │
         ▼     ▼     ▼
┌────────────────────────┐
│      Embedding         │  map token ids → dense vectors
└───────────┬────────────┘
            ▼
┌────────────────────────┐
│  Transformer Layer ×N  │  self-attention + feed-forward
└───────────┬────────────┘
            ▼
┌────────────────────────┐
│   Logits (vocab size)  │  unnormalised scores over all tokens
└───────────┬────────────┘
            ▼
┌────────────────────────┐
│  Sampling / Argmax     │  pick next token
└───────────┬────────────┘
            ▼
           [on]  ← append to sequence, repeat
```

This loop continues until a stop condition is reached.

---

## Attention (the core primitive)

Transformers rely on self-attention:

- Every token attends to every other token in the sequence
- Attention weights determine relevance between tokens

```text
Sequence: [The] [cat] [sat] [on] [the] [___]

Attention from [___] to all previous tokens:

[The]  ░░░░░░░░░░░░░░░░░░             low
[cat]  ████████████████████████████   high  ← subject
[sat]  ██████████████████████         med   ← verb
[on]   ████████████████████████████   high  ← preposition
[the]  ███████████████                med   ← article
                                      ────────────►
                                       attention weight
```

Intuition:
Instead of fixed rules, the model dynamically decides:

> “Which previous tokens matter for predicting the next one?”

This is why LLMs can:

- track long dependencies
- follow instructions
- mimic structure (e.g. code, JSON)

---

## Sampling controls (beyond temperature)

Temperature is only one lever. Others include:

- **top-k**: restrict sampling to the k most likely tokens
- **top-p (nucleus sampling)**: restrict to smallest set of tokens whose cumulative probability ≥ p
- **frequency / presence penalties**: discourage repetition

These directly affect:

- determinism
- verbosity
- hallucination rate

```text
Logits after softmax (probability distribution over vocab):

token   prob     temperature=0.2         temperature=1.0
─────   ─────    ──────────────────      ──────────────────
"mat"   0.45     █████████████████       █████████
"rug"   0.25     ████████░░░░░░░░░       █████
"bed"   0.15     ████░░░░░░░░░░░░░       ███
"hat"   0.10     ██░░░░░░░░░░░░░░░       ██
"sky"   0.05     ░░░░░░░░░░░░░░░░░       █
                 ▲ concentrated          ▲ spread out
                 (nearly deterministic) (more creative)

With top-k=3:    only [mat, rug, bed] are candidates
With top-p=0.85: only [mat, rug, bed] (cumulative 0.85)
```

---

## Why hallucinations happen

LLMs optimise for:

> “What token is statistically likely next?”

—not:

> “What is true?”

So they will:

- confidently generate plausible but incorrect information
- fill gaps when context is missing
- prefer fluency over factuality

Mitigations:

- better prompting
- retrieval (RAG)
- constrained decoding
- fine-tuning

---

## Fine-tuning vs prompting vs RAG

Three different levers:

- **prompting**: steer behaviour at runtime (cheap, flexible)
- **fine-tuning**: modify weights (expensive, persistent)
- **[RAG](https://arxiv.org/abs/2005.11401) (retrieval-augmented generation)**: inject external knowledge at inference

Rule of thumb:

- behaviour → prompt
- knowledge → RAG
- style/consistency → fine-tune

## Harnesses

To practically interface with LLMs, we build applications around them, called harnesses. A harness contains the LLM's probabilistic behaviour, enhances it and steers it towards deterministic outcomes.

A good harness has:

- a loop to feed a continuous conversation into the model
- configuration options or an interface, for customizing model behaviour
- observability, to allow users to adjust their inputs based on how the model responds
- a toolset, to allow the model to perform tasks

```text
┌─────────────────────────────────────────────────┐
│                   Harness                       │
│                                                 │
│   ┌───────────┐    ┌───────────┐    ┌─────────┐ │
│   │  Config   │    │    LLM    │    │  Tools  │ │
│   │ (prompts, │───▶│  (infer)  │───▶│ (act on │ │
│   │  params)  │    │           │    │  world) │ │
│   └───────────┘    └─────┬─────┘    └────┬────┘ │
│                          │               │      │
│                    ┌─────▼───────────────▼───┐  │
│                    │     Conversation loop   │  │
│                    │  (accumulate + re-send) │  │
│                    └────────────┬────────────┘  │
│                                 │               │
│                    ┌────────────▼────────────┐  │
│                    │     Observability       │  │
│                    │  (logs, metrics, traces)│  │
│                    └─────────────────────────┘  │
└─────────────────────────────────────────────────┘
```

The diagram above describes a chatbot harness — user input in, text out. An agent harness adds a feedback path: when the model's output contains tool calls, the harness executes them and appends the results to the conversation before the next inference call.

This feedback path makes the harness a loop. The loop introduces several concerns that a single-turn harness does not have:

- **streaming**: tokens arrive incrementally, but tool calls must be fully assembled before execution
- **interrupts**: users need to be able to abort a loop heading in the wrong direction, and external systems may need to preempt it with urgent events — the loop must support pause, yield, and resume
- **context growth**: each tool call and result adds tokens to the transcript, which will eventually exceed the context window
- **concurrency**: independent tool calls benefit from parallel execution, but the model needs all results before it can continue
- **safety**: the model can request arbitrary actions — the harness must decide which ones to permit

```text
Chat harness (open loop):

  User ──▶ Model ──▶ Text ──▶ User


Agent harness (closed loop):

  User ──▶ Model ──┬──▶ Text ──▶ User
                   │
                   ├──▶ Tool call
                   │       │
                   │    Execute
                   │       │
                   │    Result
                   │       │
                   └───────┘  ← feed back, model continues
```

---

## From harness to toolkit

A minimal agent loop is straightforward to implement. Handling all of the above — and composing cleanly into different host applications (a CLI, a web server, a multi-agent system) — requires deliberate decomposition.

`agentkit` splits the agent harness into independent crates, each responsible for one concern:

| Concern                    | agentkit crate                                                                                                                                                                                     |
| -------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Transcript data model      | [`agentkit-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-core)                                                                                                            |
| Agent loop and driver      | [`agentkit-loop`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-loop)                                                                                                            |
| Tool abstraction           | [`agentkit-tools-core`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tools-core)                                                                                                |
| Filesystem and shell tools | [`agentkit-tool-fs`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tool-fs), [`agentkit-tool-shell`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-tool-shell) |
| Permission system          | [`agentkit-capabilities`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-capabilities)                                                                                            |
| Context loading            | [`agentkit-context`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-context)                                                                                                      |
| Transcript compaction      | [`agentkit-compaction`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-compaction)                                                                                                |
| MCP integration            | [`agentkit-mcp`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-mcp)                                                                                                              |
| Task management            | [`agentkit-task-manager`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-task-manager)                                                                                            |
| Observability              | [`agentkit-reporting`](https://github.com/danielkov/agentkit/tree/main/crates/agentkit-reporting)                                                                                                  |
| Provider adapters          | [`agentkit-provider-*`](https://github.com/danielkov/agentkit/tree/main/crates)                                                                                                                    |

Each crate can be used independently. The core loop is agnostic to the model provider, tool set, and presentation layer. The rest of this book builds up each piece, starting from the loop itself.

[Chapter 2: What is an agent loop? →](./ch02-what-is-an-agent-loop.md)
