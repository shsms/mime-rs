# Legacy MCP wire fixtures

Frozen responses of `mime --mcp` to legacy (initialize-based) requests, compared
byte-for-byte by `legacy_wire_format_is_frozen` in `tests/mcp.rs` after
normalising environment-dependent fields (`roots`, `audit`).

Regenerate only when a change to the legacy format is intended:

    MIME_UPDATE_FIXTURES=1 cargo test --test mcp legacy_wire_format_is_frozen

and review the diff before committing.
