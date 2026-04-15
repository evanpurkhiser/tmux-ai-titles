use std::collections::HashMap;
use std::process::Command;
use std::thread;
use std::time::Duration;

const CLAUDE_PATH: &str = "/Users/evan/.local/bin/claude";
const LINE_THRESHOLD: usize = 200;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const CAPTURE_LINES: usize = 500;

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

fn main() {
    eprintln!("pane-title-daemon: starting");

    let mut states: HashMap<String, PaneState> = HashMap::new();

    loop {
        let panes = list_panes();

        // Clean up state for panes that no longer exist
        let active_ids: Vec<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
        states.retain(|id, _| active_ids.contains(&id.as_str()));

        for pane in &panes {
            let state = states.entry(pane.pane_id.clone()).or_insert(PaneState {
                history_size: pane.history_size,
                last_generated_at: pane.history_size,
            });

            state.history_size = pane.history_size;
            let lines_since = pane.history_size.saturating_sub(state.last_generated_at);

            if lines_since >= LINE_THRESHOLD {
                eprintln!(
                    "pane-title-daemon: generating title for {} ({lines_since} new lines)",
                    pane.pane_id
                );

                if let Some(buffer) = capture_pane(&pane.pane_id) {
                    if let Some(title) = generate_title(&buffer) {
                        set_pane_title(&pane.pane_id, &title);
                        eprintln!("pane-title-daemon: {} -> {:?}", pane.pane_id, title);
                    }
                }

                state.last_generated_at = pane.history_size;
            }
        }

        thread::sleep(POLL_INTERVAL);
    }
}
