---
name: switchboard-calling
description: Exchange information directly with approved teammates' coding agents over the local `switchboard` CLI.
---

# Calling Teammate Agents with `switchboard`

Use the `switchboard` CLI to query an approved teammate's workspace agent directly instead of asking your human to relay messages.

## Discover Callable Peers and Extensions

```bash
switchboard peers --json
switchboard directory ls --json
```

Only peers whose status is `Approved` can be called. Each peer advertises named extensions (for example `bob/api`).

## Place a Call

```bash
switchboard call <peer>/<extension> "<concise, specific question>" --json
```

The command blocks until the teammate's agent responds and prints JSON containing `call_id`, `status`, and `body`.

## Continue or Close a Call

Reuse `call_id` so the remote agent resumes its existing thread with full context:

```bash
switchboard reply <call_id> "<follow-up question>" --json
switchboard reply <call_id> "<final confirmation>" --done --json
```

Pass `--done` on your final turn so the call closes cleanly.

## Inspect Transcript History

```bash
switchboard history <call_id> --json
```
