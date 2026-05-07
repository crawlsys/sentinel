#!/usr/bin/env bash
# Sentinel + Marketplace one-command bootstrap installer.
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/garysomerhalder/sentinel/main/scripts/bootstrap.sh | bash
#
# Or after cloning sentinel manually:
#   ./scripts/bootstrap.sh
#
# What it does:
#   1. Verifies prerequisites (git, cargo, gh)
#   2. Clones the 4 dependency repos under ~/Documents/GitHub/
#      (vulcan-mcp-sdk-rust, sentinel, linear-mcp-rust, linear-cli-rust)
#   3. Clones claude-code-marketplace into ~/.claude (skipped if dir exists)
#   4. Builds each repo via `cargo build --release`
#   5. Copies sentinel + sentinel-engine + linear-mcp + linear binaries
#      from target/release/ into ~/.cargo/bin/ (handles the launcher/engine split)
#   6. Copies sentinel config templates (hooks.toml, workflows.toml) into
#      ~/.claude/sentinel/config/
#   7. Writes a starter ~/.claude/sentinel/config/settings.json with the
#      seven event handlers wired and placeholders for API keys
#   8. Writes a starter ~/.claude.json (only if it doesn't already exist)
#   9. Prints next steps
#
# Re-run-safe: every step checks for existing state before clobbering.
# Override paths with SENTINEL_GH_DIR / SENTINEL_CLAUDE_DIR / SENTINEL_GH_OWNER.

set -euo pipefail

# -- helpers ------------------------------------------------------------------

c_red()    { printf '\033[31m%s\033[0m' "$*"; }
c_green()  { printf '\033[32m%s\033[0m' "$*"; }
c_yellow() { printf '\033[33m%s\033[0m' "$*"; }
c_blue()   { printf '\033[34m%s\033[0m' "$*"; }

step() { echo; echo "$(c_blue '==>') $(c_blue "$*")"; }
ok()   { echo "    $(c_green '[ok]') $*"; }
warn() { echo "    $(c_yellow '[!! ]') $*"; }
err()  { echo "    $(c_red '[xx]') $*" >&2; }

require() {
  command -v "$1" >/dev/null 2>&1 || {
    err "missing required tool: $1"
    err "install: $2"
    exit 1
  }
}

# -- paths --------------------------------------------------------------------

GH_DIR="${SENTINEL_GH_DIR:-$HOME/Documents/GitHub}"
CLAUDE_DIR="${SENTINEL_CLAUDE_DIR:-$HOME/.claude}"
CONFIG_DIR="$CLAUDE_DIR/sentinel/config"
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"

OWNER="${SENTINEL_GH_OWNER:-garysomerhalder}"

REPOS=(
  "vulcan-mcp-sdk-rust"
  "sentinel"
  "linear-mcp-rust"
  "linear-cli-rust"
)

clone_repo() {
  local repo="$1" dest="$2"
  if [[ -d "$dest/.git" ]]; then
    ok "$repo already cloned at $dest"
    return
  fi
  if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    gh repo clone "$OWNER/$repo" "$dest"
  else
    git clone "https://github.com/$OWNER/$repo" "$dest"
  fi
  ok "$repo cloned"
}

install_binary() {
  local src="$1" name="$2"
  if [[ -f "$src" ]]; then
    install -m 0755 "$src" "$CARGO_BIN/$name"
    ok "installed $name -> $CARGO_BIN/$name"
  else
    warn "binary not found at $src (skipped)"
  fi
}

# -- main ---------------------------------------------------------------------

step "Checking prerequisites"
require git   "https://git-scm.com/downloads"
require cargo "https://rustup.rs"
ok "git and cargo present"
if command -v gh >/dev/null 2>&1; then
  if gh auth status >/dev/null 2>&1; then
    ok "gh authenticated"
  else
    warn "gh present but not authenticated; private clones will fall back to https (you'll be prompted for credentials)"
  fi
else
  warn "gh not installed; private clones will use https (slower but works)"
fi

mkdir -p "$GH_DIR" "$CARGO_BIN"

step "Cloning code repos under $GH_DIR"
for repo in "${REPOS[@]}"; do
  clone_repo "$repo" "$GH_DIR/$repo"
done

step "Cloning claude-code-marketplace into $CLAUDE_DIR"
if [[ -d "$CLAUDE_DIR/.git" ]]; then
  ok "marketplace already present at $CLAUDE_DIR"
elif [[ -d "$CLAUDE_DIR" ]] && [[ -n "$(ls -A "$CLAUDE_DIR" 2>/dev/null || true)" ]]; then
  warn "$CLAUDE_DIR exists and is non-empty but not a git repo"
  warn "back it up and re-run, or set SENTINEL_CLAUDE_DIR to a different path"
else
  clone_repo "claude-code-marketplace" "$CLAUDE_DIR"
fi

step "Building vulcan-mcp-sdk-rust (path-dep for sentinel + linear-mcp)"
( cd "$GH_DIR/vulcan-mcp-sdk-rust" && cargo build --release )
ok "vulcan built"

# vulcan ships an mcp-router binary too; install it if present.
install_binary "$GH_DIR/vulcan-mcp-sdk-rust/target/release/mcp-router"     mcp-router
install_binary "$GH_DIR/vulcan-mcp-sdk-rust/target/release/mcp-router.exe" mcp-router.exe

step "Building sentinel"
( cd "$GH_DIR/sentinel" && cargo build --release )
ok "sentinel built"

# Sentinel uses a launcher/engine split: small `sentinel` launcher + larger
# `sentinel-engine` doing the actual work. Both must end up in ~/.cargo/bin.
install_binary "$GH_DIR/sentinel/target/release/sentinel"            sentinel
install_binary "$GH_DIR/sentinel/target/release/sentinel.exe"        sentinel.exe
install_binary "$GH_DIR/sentinel/target/release/sentinel-engine"     sentinel-engine
install_binary "$GH_DIR/sentinel/target/release/sentinel-engine.exe" sentinel-engine.exe

step "Building linear-mcp"
( cd "$GH_DIR/linear-mcp-rust" && cargo build --release )
install_binary "$GH_DIR/linear-mcp-rust/target/release/linear-mcp"     linear-mcp
install_binary "$GH_DIR/linear-mcp-rust/target/release/linear-mcp.exe" linear-mcp.exe
ok "linear-mcp built and installed"

step "Building linear CLI"
( cd "$GH_DIR/linear-cli-rust" && cargo build --release )
install_binary "$GH_DIR/linear-cli-rust/target/release/linear"     linear
install_binary "$GH_DIR/linear-cli-rust/target/release/linear.exe" linear.exe
ok "linear CLI built and installed"

step "Copying sentinel config templates to $CONFIG_DIR"
mkdir -p "$CONFIG_DIR"
for f in hooks.toml workflows.toml; do
  src="$GH_DIR/sentinel/config/$f"
  dst="$CONFIG_DIR/$f"
  if [[ -f "$dst" ]]; then
    ok "$f already present (not overwriting)"
  elif [[ -f "$src" ]]; then
    cp "$src" "$dst"
    ok "$f copied"
  else
    warn "$src not found; skipping"
  fi
done

step "Writing starter settings.json"
SETTINGS="$CONFIG_DIR/settings.json"
if [[ -f "$SETTINGS" ]]; then
  ok "settings.json already present (not overwriting)"
else
  cat > "$SETTINGS" <<'JSON'
{
  "_comment": "Replace REPLACE_ME values with your own keys. Delete keys you do not use. QDRANT_* is optional; memory hooks no-op without it.",
  "env": {
    "LINEAR_API_KEY": "REPLACE_ME_FROM_https://linear.app/settings/api",
    "OPENROUTER_API_KEY": "REPLACE_ME_FROM_https://openrouter.ai/keys",
    "ANTHROPIC_API_KEY": "OPTIONAL_https://console.anthropic.com",
    "QDRANT_URL": "OPTIONAL_https://your-cluster.qdrant.tech",
    "QDRANT_API_KEY": "OPTIONAL_replace_or_remove",
    "CLAUDE_CODE_DISABLE_TERMINAL_TITLE": "1",
    "CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS": "1",
    "ENABLE_LSP_TOOL": "1"
  },
  "permissions": { "defaultMode": "default" },
  "hooks": {
    "SessionStart":     [{ "matcher": "", "hooks": [{ "type": "command", "command": "sentinel hook --event SessionStart",     "statusMessage": "Initializing marketplace..." }] }],
    "SessionEnd":       [{ "matcher": "", "hooks": [{ "type": "command", "command": "sentinel hook --event SessionEnd",       "async": true }] }],
    "UserPromptSubmit": [{ "matcher": "", "hooks": [{ "type": "command", "command": "sentinel hook --event UserPromptSubmit" }] }],
    "PreToolUse":       [{ "matcher": "", "hooks": [{ "type": "command", "command": "sentinel hook --event PreToolUse" }] }],
    "PostToolUse":      [{ "matcher": "", "hooks": [{ "type": "command", "command": "sentinel hook --event PostToolUse" }] }],
    "Stop":             [{ "matcher": "", "hooks": [{ "type": "command", "command": "sentinel hook --event Stop" }] }],
    "PreCompact":       [{ "matcher": "", "hooks": [{ "type": "command", "command": "sentinel hook --event PreCompact" }] }]
  }
}
JSON
  ok "settings.json written to $SETTINGS"
fi

step "Registering MCP servers in ~/.claude.json"
MCP_JSON="$HOME/.claude.json"
if [[ -f "$MCP_JSON" ]]; then
  ok "~/.claude.json already exists; not modifying"
  cat <<EXISTING

  Add these entries manually under "mcpServers" if they aren't already there:

      "sentinel":  { "command": "sentinel", "args": ["mcp"], "type": "stdio" },
      "linear":    { "command": "linear-mcp", "type": "stdio" }

EXISTING
else
  cat > "$MCP_JSON" <<'JSON'
{
  "mcpServers": {
    "sentinel": { "command": "sentinel", "args": ["mcp"], "type": "stdio" },
    "linear":   { "command": "linear-mcp", "type": "stdio" }
  }
}
JSON
  ok "~/.claude.json created with sentinel + linear MCP servers"
fi

# -- done ---------------------------------------------------------------------

cat <<DONE

==============================================================
  Sentinel bootstrap complete
==============================================================

Next steps:

  1. Edit your API keys:
       \$EDITOR $SETTINGS

     Required for full functionality:
       - LINEAR_API_KEY        (your own from https://linear.app/settings/api)
       - OPENROUTER_API_KEY    (your own from https://openrouter.ai/keys)
                                or ANTHROPIC_API_KEY (your own)

     Optional:
       - QDRANT_URL + QDRANT_API_KEY  (memory hooks no-op without them)

  2. Verify the binaries:
       sentinel --version
       linear-mcp --help
       linear --help

  3. Launch Claude Code with sentinel's settings:
       claude --settings $SETTINGS

  4. (Optional) Use mcp-router for hot-reloadable MCPs. Replace the
     "sentinel" / "linear" entries in ~/.claude.json with:
       "command": "mcp-router", "args": ["--single", "sentinel", "mcp"]
       "command": "mcp-router", "args": ["--single", "linear-mcp"]

DONE
