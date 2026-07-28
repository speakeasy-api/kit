# how-cli

`how` is a small terminal assistant that turns a natural-language task into one or more shell commands, lets you pick the best one, and then runs it.

It is built on [`agentkit`](https://github.com/danielkov/agentkit) and currently uses OpenRouter as its model backend.

## What it does

- accepts a prompt from interactive input, CLI args, or stdin
- asks the model for 1 to 3 concrete shell command alternatives
- checks command availability with a built-in `is_available` tool
- shows a lightweight picker in TTY mode
- executes the selected command immediately
- falls back to plain stdout output in non-interactive contexts

## Demo

<video src="./assets/demo-2.mov" controls muted playsinline></video>

[Open the demo recording](./assets/demo-2.mov)

## Install

From crates.io:

```sh
cargo install how-cli
```

From this repository:

```sh
cargo install --path projects/how
```

The installed binary name is `how`.

## Configuration

`how` reads its OpenRouter configuration from the environment:

```sh
export OPENROUTER_API_KEY=your_key_here
export OPENROUTER_MODEL=anthropic/claude-sonnet-4
```

You can also keep these in a `.env` file.

## Usage

Interactive mode:

```sh
how
```

Prompt passed as arguments:

```sh
how find the 20 largest files under the current directory
```

Prompt from stdin:

```sh
git diff --stat | how "summarize what changed and suggest the next inspection command"
```

Use a different model for one run:

```sh
how --model openrouter/hunter-alpha find every rust file that mentions prompt cache
```

## Interaction model

When `stdin` and `stderr` are attached to a terminal, `how` runs in TUI mode:

1. show a prompt if no CLI prompt was provided
2. display a small spinner while the model thinks
3. show 1 to 3 candidate commands
4. let you pick with `↑` / `↓` or `j` / `k`
5. run the selected command

Controls:

- `enter` submits the prompt or selects a command
- `esc` cancels the command picker
- `ctrl+c` cancels generation while the model is thinking

When running in a pipe or other non-interactive context, `how` prints the
candidate commands to stdout instead of opening the picker.

## Notes

- The model is asked to return shell commands only, with no prose.
- The built-in `is_available` tool lets the model avoid suggesting commands that are not installed on the current machine.
- Prompt caching is enabled with a short retention policy and a prompt-derived cache key so retries of similar requests can reuse provider-side cache state without collapsing unrelated prompts together.
