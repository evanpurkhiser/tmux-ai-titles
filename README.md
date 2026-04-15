# tmux-ai-titles

A lightweight daemon that automatically generates descriptive titles for tmux panes and windows using AI.

## How it works

The daemon polls all tmux panes every 5 seconds, hashing the last 50 lines of each pane to detect changes.

**Pane titles**: On first sight, it captures the last 500 lines from the pane buffer, sends them to Claude CLI (`claude -p --model haiku`), and generates a 4-5 word title. Subsequent regenerations happen when content has changed and 5 minutes have passed since the change.

**Window titles**: After pane titles are generated, the daemon collects all pane titles within each window and generates a 1-2 word window title that captures the overall theme.

A braille spinner animation is shown in the pane border while titles are being generated, preserving the existing title.

## Requirements

- tmux 3.2+
- [Claude CLI](https://docs.anthropic.com/en/docs/claude-code) installed and authenticated

## Installation

```bash
cargo build --release
cp target/release/tmux-ai-titles ~/.local/bin/
```

## tmux configuration

Add to your `tmux.conf`:

```tmux
# Enable pane border titles
set -g pane-border-status top
set -g pane-border-format "#{pane_id} #{pane_title}"

# Prevent programs from overwriting AI-generated titles
set -g allow-set-title off

# Start the daemon when tmux launches
run-shell "pgrep -f tmux-ai-titles >/dev/null || tmux-ai-titles &"
```

## Configuration

Currently configured via constants in `src/main.rs`:

| Constant | Default | Description |
|---|---|---|
| `POLL_INTERVAL` | 5s | How often to poll panes for changes |
| `REGEN_DELAY` | 300s | Time after content change before regenerating |
| `CAPTURE_LINES` | 500 | Lines of buffer to send for pane title generation |
| `HASH_LINES` | 50 | Lines of buffer to hash for change detection |
