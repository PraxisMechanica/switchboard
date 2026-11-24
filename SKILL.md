---
name: switchboard-calling
description: Exchange text-only messages directly with pre-existing coding agent sessions on approved teammates' machines over the local `switchboard` CLI.
---

# Messaging Pre-Existing Teammate Agents with `switchboard`

Switchboard routes plain-text messages to **pre-existing** Codex agent sessions only. It never spawns new agent sessions, never runs shell commands, and never edits files on remote calls.

## Discover Peers, Extensions, and Local Sessions

```bash
switchboard peers --json
switchboard directory ls --json
switchboard ext sessions --json
```

Only peers whose status is `Approved` can be called.

## Message a Bound Extension or Arbitrary Pre-Existing Session

```bash
# Call a teammate's named extension (bound to an existing session)
switchboard call <peer>/<extension> "<concise, specific question>" --json

# Or message ANY arbitrary pre-existing Codex session directly by its thread UUID
switchboard call <peer>/<session_uuid> "<concise, specific question>" --json
```

## Continue or Close a Call

```bash
switchboard reply <call_id> "<follow-up question>" --json
switchboard reply <call_id> "<final confirmation>" --done --json
```

## Bind a Local Extension to an Existing Session

```bash
switchboard ext bind <extension> <session_uuid>
```

## Inspect Transcript History

```bash
switchboard history <call_id> --json
```
