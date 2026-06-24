# Using mime with an MCP client

mime is a standard [MCP](https://modelcontextprotocol.io) server over **stdio**:
the command is `mime --mcp`, and it's configured entirely with environment
variables. Any MCP-capable client/harness can drive it — Claude Code, Cursor,
Cline, Continue, VS Code's agent mode, the Gemini/Codex CLIs, the language SDKs'
MCP adapters, or your own harness.

It is **self-describing**: on `initialize` it returns an `instructions` field (a
how-to-drive preamble plus a tool index), every tool carries MCP `annotations`
(`readOnlyHint` / `destructiveHint`), and `help {topic}` / `help {tool}` serve
reference on demand. So a client surfaces what the model needs from the protocol
— no client-specific setup file required.

## Install

```sh
cargo install --path .     # or `cargo install mime-rs` once published
```

This puts `mime` on your `PATH` (a C toolchain is required — see the README).

## Configuration

One environment variable matters:

- **`MIME_ROOTS`** — colon-separated directories the server is confined to (it
  refuses to open or write outside them, defaulting to the cwd). **Set it to the
  project root.** The `git_*` tools *require* it (they refuse to rewrite history
  under an implicit cwd).

## Per-client setup

Most clients take the same `{ command, args, env }` shape; only the file/command
differs.

**Claude Code** (CLI):

```sh
claude mcp add mime --scope user --env MIME_ROOTS=/path/to/project -- mime --mcp
```

**Cursor** — `~/.cursor/mcp.json` (global) or `.cursor/mcp.json` (per project):

```json
{
  "mcpServers": {
    "mime": { "command": "mime", "args": ["--mcp"], "env": { "MIME_ROOTS": "/path/to/project" } }
  }
}
```

**Cline** (VS Code extension) — its `cline_mcp_settings.json`:

```json
{
  "mcpServers": {
    "mime": { "command": "mime", "args": ["--mcp"], "env": { "MIME_ROOTS": "/path/to/project" } }
  }
}
```

**VS Code** (native agent mode) — `.vscode/mcp.json`:

```json
{
  "servers": {
    "mime": { "type": "stdio", "command": "mime", "args": ["--mcp"], "env": { "MIME_ROOTS": "${workspaceFolder}" } }
  }
}
```

**Continue** — `~/.continue/config.yaml`:

```yaml
mcpServers:
  - name: mime
    command: mime
    args: ["--mcp"]
    env:
      MIME_ROOTS: /path/to/project
```

**Any other client** (Gemini/Codex CLIs, SDK adapters, custom harness): point it
at the stdio command `mime --mcp` with `MIME_ROOTS` in the environment. That's
all the server needs.

## Over HTTP

For a hosted or multi-client setup, run the server over Streamable HTTP instead
of stdio:

```sh
MIME_ROOTS=/path/to/project mime --http            # binds 127.0.0.1:7711
MIME_ROOTS=/path/to/project mime --http 127.0.0.1:9000   # or a chosen address
```

Then point an HTTP-capable client at the endpoint `http://127.0.0.1:7711/mcp`:

```json
{
  "mcpServers": {
    "mime": { "type": "streamable-http", "url": "http://127.0.0.1:7711/mcp" }
  }
}
```

It speaks JSON over `POST /mcp` (no SSE — mime never server-initiates). The
server binds localhost and rejects a non-local browser `Origin`; each client is
isolated by the `Mcp-Session-Id` it's issued on `initialize`. Verify it the same
way: `npx @modelcontextprotocol/inspector --cli http://127.0.0.1:7711/mcp
--method tools/list`.

## Verifying a connection

The official [MCP Inspector](https://github.com/modelcontextprotocol/inspector)
is the quickest conformance check:

```sh
MIME_ROOTS=/path/to/project npx @modelcontextprotocol/inspector mime --mcp
```

It runs the `initialize` handshake, lists the tools (with their annotations and
the server `instructions`), and lets you invoke a tool by hand.
