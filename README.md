# tmux-ai-pane-title

A lightweight daemon that automatically generates short, descriptive titles for tmux panes based on their terminal content.

## How it works

The daemon polls all tmux panes every 5 seconds, tracking the scroll history size of each pane. When a pane accumulates 200+ new lines since the last title generation, it:

1. Captures the last 500 lines from the pane buffer
2. Sends them to Claude CLI (`claude -p`) to generate a 4-5 word title
3. Sets the title on the pane via `tmux select-pane -T`

The generated title is available in tmux as `#{pane_title}`, which can be used in `pane-border-format` or anywhere else tmux supports format strings.

## Requirements

- tmux 3.2+
- [Claude CLI](https://docs.anthropic.com/en/docs/claude-code) installed and authenticated

## Installation

```bash
cargo build --release
cp target/release/pane-title-daemon ~/.local/bin/
```

## tmux configuration

Add to your `tmux.conf`:

```tmux
# Enable pane border titles
set -g pane-border-status top
set -g pane-border-format "#{pane_index} #{pane_title}"

# Start the daemon when tmux launches
run-shell "pgrep -f pane-title-daemon >/dev/null || pane-title-daemon &"
```

## Configuration

Currently configured via constants in `src/main.rs`:

| Constant | Default | Description |
|---|---|---|
| `LINE_THRESHOLD` | 200 | New lines before regenerating a title |
| `POLL_INTERVAL` | 5s | How often to check pane history sizes |
| `CAPTURE_LINES` | 500 | Lines of buffer to send for title generation |
