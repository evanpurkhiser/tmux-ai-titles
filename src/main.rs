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

struct PaneState {
    last_hash: u64,
    last_changed: Instant,
    last_generated: Instant,
    has_generated: bool,
}

fn hash_buffer(buffer: &str) -> u64 {
    // Hash the last HASH_LINES lines
    let lines: Vec<&str> = buffer.lines().rev().take(HASH_LINES).collect();
    let mut hasher = DefaultHasher::new();
    lines.hash(&mut hasher);
    hasher.finish()
}

fn list_pane_ids() -> Vec<String> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_id}"])
        .output()
        .ok();

    let Some(output) = output else { return vec![] };
    let stdout = String::from_utf8_lossy(&output.stdout);

    stdout.lines().map(|s| s.to_string()).collect()
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

fn generate_title(buffer: &str) -> Option<String> {
    let mut child = Command::new(CLAUDE_PATH)
        .args([
            "-p",
            "--model", "haiku",
            "Based on this terminal output, generate a 4-5 word title describing what this terminal pane is being used for. Output ONLY the title, nothing else. No quotes, no punctuation.",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    use std::io::Write;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(buffer.as_bytes()).ok();
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

fn get_pane_title(pane_id: &str) -> String {
    Command::new("tmux")
        .args(["display-message", "-t", pane_id, "-p", "#{pane_title}"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn spawn_title_generation(pane_id: String, buffer: String) {
    thread::spawn(move || {
        let done = Arc::new(AtomicBool::new(false));
        let done_clone = done.clone();
        let pane_id_clone = pane_id.clone();

        // Get current title to preserve during loading
        let current_title = get_pane_title(&pane_id);

        // Spinner thread
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

        // Generate title
        let title = generate_title(&buffer);
        done.store(true, Ordering::Relaxed);
        spinner.join().ok();

        if let Some(title) = title {
            set_pane_title(&pane_id, &title);
            eprintln!("{} -> {}", pane_id, title);
        }
    });
}

fn main() {
    eprintln!("pane-title-daemon: starting");

    let mut states: HashMap<String, PaneState> = HashMap::new();

    loop {
        let pane_ids = list_pane_ids();

        // Clean up state for panes that no longer exist
        states.retain(|id, _| pane_ids.contains(id));

        let now = Instant::now();

        for pane_id in &pane_ids {
            // Capture a small snippet for hashing
            let Some(snippet) = capture_pane(pane_id, HASH_LINES) else {
                continue;
            };

            let hash = hash_buffer(&snippet);
            let is_new = !states.contains_key(pane_id);

            let state = states.entry(pane_id.clone()).or_insert(PaneState {
                last_hash: hash,
                last_changed: now,
                last_generated: now,
                has_generated: false,
            });

            // Update last_changed if content changed
            if hash != state.last_hash {
                state.last_changed = now;
                state.last_hash = hash;
            }

            // Generate on first sight, or when content has been stable
            // for REGEN_DELAY after changing since last generation
            let should_generate = if is_new {
                true
            } else if !state.has_generated {
                true
            } else {
                state.last_changed > state.last_generated
                    && now.duration_since(state.last_changed) >= REGEN_DELAY
            };

            if should_generate {
                // Capture full buffer for title generation
                if let Some(buffer) = capture_pane(pane_id, CAPTURE_LINES) {
                    spawn_title_generation(pane_id.clone(), buffer);
                }
                state.last_generated = now;
                state.has_generated = true;
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}
