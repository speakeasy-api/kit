# Introduction

This book is a technical guide to building LLM agent applications in Rust. It uses agentkit — a modular toolkit split into small, composable crates — as both the teaching vehicle and a production-ready library you can integrate into your own projects.

It is a progressive walkthrough of the design decisions, trade-offs, and implementation patterns behind a working agent system. By the end, you should be able to:

- Integrate agentkit into your own applications
- Understand _why_ each abstraction exists and what alternatives were considered
- Build your own agent toolkit from scratch if you prefer

## What agentkit is

`agentkit` is a Rust toolkit for building LLM agent applications: coding agents, assistant CLIs, multi-agent orchestration tools, and anything else that runs a model in a loop with tools.

The project is split into small crates behind feature flags. You pull in only what you need. The core loop is runtime-agnostic. Tool crates, MCP integration, and provider adapters add functionality at the edges.

## How this book is structured

The book follows the dependency graph of a real agent system, bottom-up:

**Part I: The agent loop** starts with the fundamental question — what is an agent loop? — and builds up from transcript types through streaming, model adapters, the driver, and interrupt-based control flow. This is the foundation everything else rests on.

**Part II: Tools and safety** introduces the capability and tool abstraction layers, the permission system, built-in filesystem and shell tools, and how to write your own. Safety is a first-class concern, not an afterthought.

**Part III: Context, compaction, and memory** covers how agents load project context and how to manage transcript growth through compaction strategies.

**Part IV: Integration and extensibility** covers MCP server integration, exposing agents to editors and other clients over the Agent Client Protocol (ACP), async task management for parallel tool execution, reporting and observability, and provider adapter implementation.

**Part V: Building a coding agent** ties everything together by walking through the architecture of a complete coding agent — the kind of tool you use every day when you use Claude Code or Codex CLI.

## Who this is for

This book assumes you are comfortable with Rust and have a working understanding of async programming. You do not need prior experience with LLM APIs, but familiarity with the basic concept of chat completions (system/user/assistant messages, tool calling) will help.

If you are evaluating agent frameworks, this book will give you enough depth to make an informed decision. If you are building your own agent system, it covers the design constraints you are likely to encounter.
