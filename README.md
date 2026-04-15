# tmux-ai-titles

A lightweight daemon that automatically generates descriptive titles for tmux panes and windows using AI.

## How it works

The daemon polls all tmux panes every 5 seconds, hashing the last 50 lines of each pane to detect changes.

**Pane titles**: On first sight, it captures the last 500 lines from the pane buffer, sends them to Claude CLI (`claude -p --model haiku`), and generates a 4-5 word title. Subsequent regenerations happen when content has changed and 5 minutes have passed since the change.

**Window titles**: After pane titles are generated, the daemon collects all pane titles and their working directories within each window and generates a 1-2 word window title that captures the overall theme.

A braille spinner animation is shown in the pane border while titles are being generated, preserving the existing title.

## Requirements

- tmux 3.2+
- [Claude CLI](https://docs.anthropic.com/en/docs/claude-code) installed and authenticated

## Installation

### Homebrew (macOS)

```bash
brew install evanpurkhiser/personal/tmux-ai-titles
```

### AUR (Arch Linux)

```bash
yay -S tmux-ai-titles     # build from source
yay -S tmux-ai-titles-bin # prebuilt binary
```

### Cargo

```bash
cargo install tmux-ai-titles
```

### From source

```bash
cargo build --release
cp target/release/tmux-ai-titles ~/.local/bin/
```

## Usage

```bash
# Start the daemon
tmux-ai-titles start

# Check if it's running
tmux-ai-titles status

# Force regeneration of all titles
tmux-ai-titles regenerate

# Stop the daemon
tmux-ai-titles stop
```

The daemon manages a PID file in `$XDG_RUNTIME_DIR` (or `/tmp`) to ensure only one instance runs at a time.

## tmux configuration

Add to your `tmux.conf`:

```tmux
# Enable pane border titles
set -g pane-border-status top
set -g pane-border-format "#{pane_id} #{pane_title}"

# Prevent programs from overwriting AI-generated titles
set -g allow-set-title off

# Start the daemon when tmux launches
run-shell "tmux-ai-titles start &"
```

## Options

All options are passed to the `start` subcommand:

| Flag | Default | Description |
|---|---|---|
| `--poll-interval` | 5 | How often to poll panes for changes (seconds) |
| `--regenerate-delay` | 300 | Seconds after content change before regenerating |
| `--capture-lines` | 500 | Lines of buffer to send for pane title generation |
| `--hash-lines` | 50 | Lines of buffer to hash for change detection |
| `--model` | haiku | Claude model to use |
| `--no-pane-titles` | | Disable pane title generation (window titles still work) |
| `--no-window-titles` | | Disable window title generation |
