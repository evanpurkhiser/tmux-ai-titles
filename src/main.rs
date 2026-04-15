use std::collections::HashMap;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

const CLAUDE_PATH: &str = "/Users/evan/.local/bin/claude";
const LINE_THRESHOLD: usize = 200;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const SPINNER_INTERVAL: Duration = Duration::from_millis(80);
const CAPTURE_LINES: usize = 500;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠚", "⠞", "⠖", "⠦", "⠴", "⠲", "⠳", "⠓"];

#[derive(Debug)]
struct PaneState {
    history_size: usize,
    last_generated_at: usize,
}

struct PaneInfo {
    pane_id: String,
    history_size: usize,
}

fn list_panes() -> Vec<PaneInfo> {
    let output = Command::new("tmux")
        .args(["list-panes", "-a", "-F", "#{pane_id} #{history_size}"])
        .output()
        .ok();

    let Some(output) = output else { return vec![] };
    let stdout = String::from_utf8_lossy(&output.stdout);

    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, ' ');
            let pane_id = parts.next()?.to_string();
            let history_size: usize = parts.next()?.parse().ok()?;
            Some(PaneInfo {
                pane_id,
                history_size,
            })
        })
        .collect()
}

fn capture_pane(pane_id: &str) -> Option<String> {
    let start = -(CAPTURE_LINES as i64);
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
        Some(String::from_utf8_lossy(&output.stdout).to_string())
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
        let panes = list_panes();

        // Clean up state for panes that no longer exist
        let active_ids: Vec<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
        states.retain(|id, _| active_ids.contains(&id.as_str()));

        for pane in &panes {
            let is_new = !states.contains_key(&pane.pane_id);
            let state = states.entry(pane.pane_id.clone()).or_insert(PaneState {
                history_size: pane.history_size,
                last_generated_at: pane.history_size,
            });

            state.history_size = pane.history_size;
            let lines_since = pane.history_size.saturating_sub(state.last_generated_at);

            // Generate on first sight if pane has any content, then every 200 new lines
            let should_generate = pane.history_size > 0
                && (is_new || lines_since >= LINE_THRESHOLD);

            if should_generate {
                if let Some(buffer) = capture_pane(&pane.pane_id) {
                    spawn_title_generation(pane.pane_id.clone(), buffer);
                }
                state.last_generated_at = pane.history_size;
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}
