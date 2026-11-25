# Switchboard

Decentralized peer-to-peer agent switchboard in Rust. Switchboard lets coding agents on teammates' machines exchange messages directly across continents (or locked inside a VPN/Tailnet), with cryptographic peer identities, discoverable phone book cards, active human approval for peering, and zero MCP overhead.

## Architecture

Switchboard is a single Rust crate organized into **vertical domain slices under `src/`**. Each slice owns its models, pure logic, SQLite schema/repository, port traits, adapters, conformance tests, and CLI definitions:

* `src/identity/` — Ed25519 node keys (`secrecy::SecretBox`), `PeerId`, and generic `Signed<T>` / `Verified<T>` proof wrappers.
* `src/transport/` — Segregated `PeerDialer` / `PeerListener` ports, versioned length-prefixed wire framing, `IrohTransport` (global QUIC P2P + `VpnOnly` mode), `MemoryTransport`, and generic conformance tests.
* `src/directory/` — `PeerCard`, `DirectoryPort`, `GitPhonebook` (signed 6-teammate Git phone book), SQLite verified-card cache, and conformance tests.
* `src/peers/` — Pure peering state machine (`PendingInbound`, `PendingOutbound`, `Approved`, `Revoked`), proof-carrying `ApprovedPeer` token, active human approval flow, and `proptest` invariants.
* `src/extensions/` — Local callable workspace registry, `AgentHarness` port, `CodexHarness` (official OpenAI `codex app-server` JSON-RPC 2.0 `stdio` protocol), and conformance tests.
* `src/calls/` — Outbound/inbound call orchestration requiring `ApprovedPeer` at compile time, per-`CallId` turn locks, idempotent receipts (`run_idempotent`), and call transcripts.

## Quick Start

1. Initialize your node identity and configure `~/.config/switchboard/config.toml`:
   ```toml
   [directory]
   git_remote = "git@github.com:your-org/switchboard-phonebook.git"

   # Optional VPN-only lockdown (disables public relays and public discovery):
   # [network]
   # VpnOnly = { bind_addr = "100.64.0.10:0", custom_relays = [] }
   ```
   ```bash
   switchboard init --name alice
   ```
2. Register a callable workspace extension and start the daemon:
   ```bash
   switchboard ext add api --dir ~/code/api --desc "Orders HTTP API"
   switchboard daemon
   ```
3. Publish your signed peer card and sync teammates:
   ```bash
   switchboard directory publish
   switchboard directory sync
   switchboard directory ls
   ```
4. Pair with a teammate (requires active human approval):
   ```bash
   # On Alice's machine:
   switchboard dial bob -m "Frontend/backend API coordination"

   # On Bob's machine:
   switchboard ring
   switchboard approve alice
   ```
5. Message a bound extension or any arbitrary pre-existing Codex session:
   ```bash
   switchboard ext sessions
   switchboard call bob/api "What exact payload does POST /v2/orders expect?" --json
   switchboard call bob/<session_uuid> "Quick status check?" --json
   switchboard reply <call_id> "Does that also apply for EUR?" --done --json
   ```

## Development Gates

```bash
just check        # cargo fmt --check, clippy -D warnings, 500-line check, all tests
```
