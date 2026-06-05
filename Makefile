# Build, test, install, and wire mime into Claude Code.

CARGO_BIN ?= $(HOME)/.cargo/bin
# Filesystem roots the sandboxed (MCP) tier may touch, colon-separated.
# Default: the repo's parent directory — typically where all your projects
# live. Override (make claude MIME_ROOTS=/a:/b), or set it empty to confine
# each Claude Code session to its own working directory.
MIME_ROOTS ?= $(abspath ..)

.PHONY: build test install claude claude-mcp uninstall-mcp

build:
	cargo build --release

# The same gate CI runs.
test:
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

install:
	cargo install --path .

# Install + register: the one-shot Claude Code integration.
claude: install claude-mcp

# Register mime as a user-scope MCP server so every Claude Code session picks
# its tools up automatically. Idempotent: re-registration replaces the entry.
claude-mcp:
	-claude mcp remove --scope user mime >/dev/null 2>&1
	claude mcp add --scope user mime \
		$(if $(MIME_ROOTS),--env 'MIME_ROOTS=$(MIME_ROOTS)') \
		-- '$(CARGO_BIN)/mime' --mcp

uninstall-mcp:
	claude mcp remove --scope user mime
