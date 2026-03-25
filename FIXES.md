# Fix: Claude refuses to use tools via iMessage

## Problem

When a user asks Claude (via iMessage/sink) to create a file, run a command, or use any tool, Claude responds saying it "cannot execute tools" and offers to provide code as text instead. Claude believes it is a text-only responder.

## Root Cause

The prompt in `src/claude.rs:build_prompt()` (line 45) tells Claude:

```
"YOU ARE A NESTED AGENT. You are Claude, an assistant responding via iMessage."
```

And then (line 47-48):

```
"Just provide your text response to help with whatever the user asks."
"Focus on research, answers, and assistance based on the user's request."
```

Claude interprets "NESTED AGENT" + "just provide your text response" as meaning it is a **text-only** responder with no tool access. In reality, it's invoked with `--dangerously-skip-permissions` and has FULL tool access (Bash, Read, Write, Edit, WebFetch, etc.).

## The Fix

Two changes needed in `src/claude.rs:build_prompt()`:

1. **Remove "NESTED AGENT" framing** — this is what triggers Claude's self-limitation. Replace with something like "You are Claude, responding to a user via iMessage." Don't use language that implies it's sandboxed or limited.

2. **Explicitly tell Claude it HAS tool access** — add instructions like:
   - "You have full tool access: Bash, Read, Write, Edit, WebFetch, etc."
   - "Use tools freely to fulfill requests — create files, run commands, search the web, deploy code, etc."
   - "The ONLY thing you cannot do is send iMessages — the system handles message delivery. Everything else is available to you."

3. **Remove "Just provide your text response"** — this line directly causes Claude to think it should only output text. Replace with something that encourages action.

## Suggested prompt rewrite (lines 44-49)

```rust
prompt.push_str("You are Claude, responding to a user via iMessage.\n\n");
prompt.push_str("You have FULL tool access: Bash, Read, Write, Edit, WebFetch, WebSearch, and all other tools.\n");
prompt.push_str("Use tools freely to fulfill requests — create files, run commands, search the web, query databases, deploy code, etc.\n");
prompt.push_str("The ONLY restriction: Do NOT use tools to send iMessages/texts/notifications. Message delivery is handled automatically by the system.\n");
prompt.push_str("Your final text output will be sent as an iMessage reply, so keep it concise and suitable for iMessage.\n\n");
```

## Why Claude gets confused

Claude Code has built-in behavior where "nested agents" (spawned via the Agent tool) have restricted tool access by default. By labeling the iMessage Claude as a "NESTED AGENT", the prompt accidentally triggers this self-restriction behavior, even though the process was launched with full permissions via `--dangerously-skip-permissions`.
