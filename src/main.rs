use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

const CLAUDE_PATH: &str = "/Users/evan/.local/bin/claude";
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const REGEN_DELAY: Duration = Duration::from_secs(300);
const SPINNER_INTERVAL: Duration = Duration::from_millis(80);
const CAPTURE_LINES: usize = 500;
const HASH_LINES: usize = 50;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠚", "⠞", "⠖", "⠦", "⠴", "⠲", "⠳", "⠓"];

const PANE_TITLE_PROMPT: &str = "Based on the terminal output below, generate a 4-5 word title describing what this terminal pane is being used for. Output ONLY the title, nothing else. No quotes, no punctuation. The output may be empty or minimal — for idle or empty terminals, use a title like \"Idle Shell\".";

const WINDOW_TITLE_PROMPT: &str = "You are given a list of short pane titles from a tmux window. Each title describes what one terminal pane is being used for. Summarize them into a 1-2 word window title that captures the overall theme or project. Output ONLY the 1-2 word title, nothing else. No quotes, no punctuation, no explanation.";

struct PaneState {
    last_hash: u64,
    last_changed: Instant,
    last_generated: Instant,
    has_generated: bool,
}

struct WindowState {
    last_titles_hash: u64,
    last_changed: Instant,
    last_generated: Instant,
    has_generated: bool,
}

struct PaneInfo {
    pane_id: String,
    window_id: String,
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn hash_buffer(buffer: &str) -> u64 {
    let lines: Vec<&str> = buffer.lines().rev().take(HASH_LINES).collect();
    let mut hasher = DefaultHasher::new();
    lines.hash(&mut hasher);
    hasher.finish()
}

fn list_panes() -> Vec<PaneInfo> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_id} #{window_id}"])
        .output()
        .ok();

    let Some(output) = output else { return vec![] };
    let stdout = String::from_utf8_lossy(&output.stdout);

    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ' ');
            let pane_id = parts.next()?.to_string();
            let window_id = parts.next()?.to_string();
            Some(PaneInfo { pane_id, window_id })
        })
        .collect()
}

fn capture_pane(pane_id: &str, lines: usize) -> Option<String> {
    let start = -(lines as i64);
    let output = Command::new("tmux")
        .args([
            "capture-pane",
            "-t",
            pane_id,
            "-p",
            "-S",
            &start.to_string(),
        ])
        .output()
        .ok()?;

    if output.status.success() {
        let content = String::from_utf8_lossy(&output.stdout).to_string();
        let trimmed = content.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(content)
        }
    } else {
        None
    }
}

fn call_claude(prompt: &str, input: &str) -> Option<String> {
    let mut child = Command::new(CLAUDE_PATH)
        .args(["-p", "--model", "haiku", prompt])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    use std::io::Write;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(input.as_bytes()).ok();
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().ok()?;

    if output.status.success() {
        let title = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !title.is_empty() {
            return Some(title);
        }
    }
    None
}

fn set_pane_title(pane_id: &str, title: &str) {
    Command::new("tmux")
        .args(["select-pane", "-t", pane_id, "-T", title])
        .output()
        .ok();
}

fn set_window_title(window_id: &str, title: &str) {
    Command::new("tmux")
        .args(["rename-window", "-t", window_id, title])
        .output()
        .ok();
}

fn get_pane_title(pane_id: &str) -> String {
    Command::new("tmux")
        .args(["display-message", "-t", pane_id, "-p", "#{pane_title}"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn spawn_pane_title_generation(pane_id: String, buffer: String) {
    thread::spawn(move || {
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = done.clone();
        let pane_id_clone = pane_id.clone();

        let current_title = get_pane_title(&pane_id);

        let spinner = thread::spawn(move || {
            let mut frame = 0;
            while !done_clone.load(Ordering::Relaxed) {
                let spinner = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
                if current_title.is_empty() {
                    set_pane_title(&pane_id_clone, spinner);
                } else {
                    set_pane_title(&pane_id_clone, &format!("{spinner} {current_title}"));
                }
                frame += 1;
                thread::sleep(SPINNER_INTERVAL);
            }
        });

        let input = format!("```\n{buffer}\n```");
        let title = call_claude(PANE_TITLE_PROMPT, &input);
        done.store(true, Ordering::Relaxed);
        spinner.join().ok();

        if let Some(title) = title {
            set_pane_title(&pane_id, &title);
            eprintln!("pane {} -> {}", pane_id, title);
        }
    });
}

fn spawn_window_title_generation(window_id: String, pane_titles: String) {
    thread::spawn(move || {
        let title = call_claude(WINDOW_TITLE_PROMPT, &pane_titles);
        if let Some(title) = title {
            set_window_title(&window_id, &title);
            eprintln!("window {} -> {}", window_id, title);
        }
    });
}

fn main() {
    eprintln!("tmux-ai-titles: starting");

    let mut pane_states: HashMap<String, PaneState> = HashMap::new();
    let mut window_states: HashMap<String, WindowState> = HashMap::new();

    loop {
        let panes = list_panes();

        let active_pane_ids: Vec<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
        pane_states.retain(|id, _| active_pane_ids.contains(&id.as_str()));

        let active_window_ids: Vec<&str> = panes.iter().map(|p| p.window_id.as_str()).collect();
        window_states.retain(|id, _| active_window_ids.contains(&id.as_str()));

        let now = Instant::now();

        // --- Pane titles ---
        for pane in &panes {
            let Some(snippet) = capture_pane(&pane.pane_id, HASH_LINES) else {
                continue;
            };

            let hash = hash_buffer(&snippet);
            let is_new = !pane_states.contains_key(&pane.pane_id);

            let state = pane_states.entry(pane.pane_id.clone()).or_insert(PaneState {
                last_hash: hash,
                last_changed: now,
                last_generated: now,
                has_generated: false,
            });

            if hash != state.last_hash {
                state.last_changed = now;
                state.last_hash = hash;
            }

            let should_generate = if is_new || !state.has_generated {
                true
            } else {
                state.last_changed > state.last_generated
                    && now.duration_since(state.last_changed) >= REGEN_DELAY
            };

            if should_generate {
                if let Some(buffer) = capture_pane(&pane.pane_id, CAPTURE_LINES) {
                    spawn_pane_title_generation(pane.pane_id.clone(), buffer);
                }
                state.last_generated = now;
                state.has_generated = true;
            }
        }

        // --- Window titles ---
        // Group panes by window and collect their titles
        let mut window_panes: HashMap<&str, Vec<String>> = HashMap::new();
        for pane in &panes {
            let title = get_pane_title(&pane.pane_id);
            window_panes
                .entry(pane.window_id.as_str())
                .or_default()
                .push(title);
        }

        for (window_id, titles) in &window_panes {
            let combined = titles.join(", ");
            let hash = hash_str(&combined);
            let is_new = !window_states.contains_key(*window_id);

            let state = window_states
                .entry(window_id.to_string())
                .or_insert(WindowState {
                    last_titles_hash: hash,
                    last_changed: now,
                    last_generated: now,
                    has_generated: false,
                });

            if hash != state.last_titles_hash {
                state.last_changed = now;
                state.last_titles_hash = hash;
            }

            let should_generate = if is_new || !state.has_generated {
                true
            } else {
                state.last_changed > state.last_generated
                    && now.duration_since(state.last_changed) >= REGEN_DELAY
            };

            if should_generate {
                // Filter out empty and spinner titles
                let real_titles: Vec<&String> = titles
                    .iter()
                    .filter(|t| !t.is_empty() && !SPINNER_FRAMES.iter().any(|s| t.starts_with(s)))
                    .collect();

                if real_titles.is_empty() {
                    continue;
                }

                let input = real_titles
                    .iter()
                    .enumerate()
                    .map(|(i, t)| format!("{}. {}", i + 1, t))
                    .collect::<Vec<_>>()
                    .join("\n");

                spawn_window_title_generation(window_id.to_string(), input);
                state.last_generated = now;
                state.has_generated = true;
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}
