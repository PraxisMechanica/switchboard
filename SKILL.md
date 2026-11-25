---
name: switchboard-calling
description: Exchange text-only messages directly with pre-existing coding agent sessions on approved teammates' machines over the local `switchboard` CLI.
---

# Messaging Pre-Existing Teammate Agents with `switchboard`

Switchboard routes plain-text messages directly to **pre-existing** agent sessions using `<user-or-machine>.<harness>.<conversation-name-or-id>`. No per-agent registration is required, and Switchboard never spawns new sessions, never runs shell commands, and never edits files on remote calls.

## Discover Peers and Callable Session Addresses

```bash
switchboard peers --json
switchboard directory ls --json
switchboard sessions --json
```

Only peers whose status is `Approved` can be called.

## Message Any Pre-Existing Session Directly

```bash
# By session UUID:
switchboard call dante.codex.01a1f360-26fc-7273-af47-5bf87a88c890 "<concise question>" --json

# Or with a copied codex://threads/ link:
switchboard call dante.codex.codex://threads/01a1f360-26fc-7273-af47-5bf87a88c890 "<concise question>" --json

# Or by existing conversation name / slug:
switchboard call dante.codex.verify-euclid-cluster-access "<concise question>" --json
```

## Continue or Close a Call

```bash
switchboard reply <call_id> "<follow-up question>" --json
switchboard reply <call_id> "<final confirmation>" --done --json
```

## Inspect Transcript History

```bash
switchboard history <call_id> --json
```
