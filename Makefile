# Build, test, install, and wire mime into Claude Code.

CARGO_BIN ?= $(HOME)/.cargo/bin
# Filesystem roots the sandboxed (MCP) tier may touch, colon-separated.
# Default: the repo's parent directory — typically where all your projects
# live. Override (make claude MIME_ROOTS=/a:/b), or set it empty to confine
# each Claude Code session to its own working directory.
MIME_ROOTS ?= $(abspath ..)

.PHONY: build test install claude claude-exec claude-mcp uninstall-mcp docs inspector

build:
	cargo build --release

# The same gate CI runs. --features tui so the feature-gated front end
# compiles under the gate too, not only on a manual build.
test:
	cargo fmt --check
	cargo clippy --all-targets --features tui -- -D warnings
	cargo test --features tui

install:
	cargo install --path .

# Regenerate the MCP tool catalogue from the live schemas — the single
# source the README links to (no hand-synced copies).
docs:
	@mkdir -p docs
	cargo run --quiet -- describe-mcp > docs/mcp-tools.md
	@echo "wrote docs/mcp-tools.md"

# Install + register: the one-shot Claude Code integration.
claude: install claude-mcp

# The same, with the exec capability granted: git_exec_over, and the
# configured OpenPGP signer when commit.gpgsign is on.
claude-exec: MIME_EXEC = 1
claude-exec: claude

# Register mime as a user-scope MCP server so every Claude Code session picks
# its tools up automatically. Idempotent: re-registration replaces the entry.
# MIME_EXEC (empty by default; `make claude-exec` sets it) is passed through.
claude-mcp:
	-claude mcp remove --scope user mime >/dev/null 2>&1
	claude mcp add --scope user mime \
		$(if $(MIME_ROOTS),--env 'MIME_ROOTS=$(MIME_ROOTS)') \
		$(if $(MIME_EXEC),--env 'MIME_EXEC=$(MIME_EXEC)') \
		-- '$(CARGO_BIN)/mime' --mcp

uninstall-mcp:
	claude mcp remove --scope user mime

# Conformance: drive the stdio server with the official MCP Inspector
# (interactive; needs node). Use `--cli` for a scripted tools/list.
inspector: build
	npx @modelcontextprotocol/inspector target/release/mime --mcp
