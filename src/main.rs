use clap::Parser;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠚", "⠞", "⠖", "⠦", "⠴", "⠲", "⠳", "⠓"];

const PANE_TITLE_PROMPT: &str = "Based on the terminal output below, generate a 4-5 word title describing what this terminal pane is being used for. Output ONLY the title, nothing else. No quotes, no punctuation. The output may be empty or minimal — for idle or empty terminals, use a title like \"Idle Shell\".";

const WINDOW_TITLE_PROMPT: &str = "You are given a list of short pane titles from a tmux window. Each title describes what one terminal pane is being used for. Summarize them into a 1-2 word window title that captures the overall theme or project. Output ONLY the 1-2 word title, nothing else. No quotes, no punctuation, no explanation.";

/// AI-powered title generation for tmux panes and windows
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Seconds to wait after content changes before regenerating
    #[arg(long, default_value_t = 300)]
    regenerate_delay: u64,

    /// Lines of buffer to capture for title generation
    #[arg(long, default_value_t = 500)]
    capture_lines: usize,

    /// Lines of buffer to hash for change detection
    #[arg(long, default_value_t = 50)]
    hash_lines: usize,

    /// How often to poll panes for changes (seconds)
    #[arg(long, default_value_t = 5)]
    poll_interval: u64,

    /// Claude model to use
    #[arg(long, default_value = "haiku")]
    model: String,

    /// Disable pane title generation
    #[arg(long)]
    no_pane_titles: bool,

    /// Disable window title generation
    #[arg(long)]
    no_window_titles: bool,
}

/// Tracks content changes and generation timing for both panes and windows.
struct ChangeTracker {
    last_hash: u64,
    last_changed: Instant,
    last_generated: Instant,
    has_generated: bool,
}

impl ChangeTracker {
    fn new(hash: u64, now: Instant) -> Self {
        Self {
            last_hash: hash,
            last_changed: now,
            last_generated: now,
            has_generated: false,
        }
    }

    fn update_hash(&mut self, hash: u64, now: Instant) {
        if hash != self.last_hash {
            self.last_changed = now;
            self.last_hash = hash;
        }
    }

    fn should_generate(&self, now: Instant, regen_delay: Duration) -> bool {
        if !self.has_generated {
            true
        } else {
            self.last_changed > self.last_generated
                && now.duration_since(self.last_changed) >= regen_delay
        }
    }

    fn mark_generated(&mut self, now: Instant) {
        self.last_generated = now;
        self.has_generated = true;
    }
}

struct PaneInfo {
    pane_id: String,
    window_id: String,
}

/// Shared map of pane_id -> generated title, used for window title generation
type TitleMap = Arc<Mutex<HashMap<String, String>>>;

/// Set of pane/window IDs currently being processed by a background thread
type InFlight = Arc<Mutex<HashSet<String>>>;

/// RAII guard that sets an AtomicBool to true on drop, ensuring the spinner
/// stops even if the generation function panics.
struct SpinnerGuard {
    done: Arc<AtomicBool>,
}

impl Drop for SpinnerGuard {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
    }
}

fn hash_str(s: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

fn hash_buffer(buffer: &str, hash_lines: usize) -> u64 {
    let lines: Vec<&str> = buffer.lines().rev().take(hash_lines).collect();
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

fn call_claude(model: &str, prompt: &str, input: &str) -> Option<String> {
    let mut child = Command::new("claude")
        .args(["-p", "--model", model, prompt])
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

fn spawn_pane_title_generation(
    pane_id: String,
    buffer: String,
    model: Arc<str>,
    set_title: bool,
    title_map: TitleMap,
    in_flight: InFlight,
) {
    thread::spawn(move || {
        // Relaxed ordering is sufficient here: the spinner thread and the
        // generation thread share no other state through this flag — it is a
        // simple "stop" signal, so no acquire/release synchronization is needed.
        let done = Arc::new(AtomicBool::new(false));
        let _guard = SpinnerGuard { done: done.clone() };
        let pane_id_clone = pane_id.clone();

        let current_title = if set_title {
            get_pane_title(&pane_id)
        } else {
            title_map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&pane_id)
                .cloned()
                .unwrap_or_default()
        };

        // Only show spinner if we're setting pane titles
        let spinner = if set_title {
            let done_clone = done.clone();
            let pane_id_clone2 = pane_id_clone.clone();
            let current_title2 = current_title.clone();
            Some(thread::spawn(move || {
                let mut frame = 0;
                while !done_clone.load(Ordering::Relaxed) {
                    let spinner = SPINNER_FRAMES[frame % SPINNER_FRAMES.len()];
                    if current_title2.is_empty() {
                        set_pane_title(&pane_id_clone2, spinner);
                    } else {
                        set_pane_title(&pane_id_clone2, &format!("{spinner} {current_title2}"));
                    }
                    frame += 1;
                    thread::sleep(Duration::from_millis(80));
                }
            }))
        } else {
            None
        };

        let input = format!("```\n{buffer}\n```");
        let title = call_claude(&model, PANE_TITLE_PROMPT, &input);

        // SpinnerGuard will set done=true on drop, but we also set it here so
        // the spinner stops promptly on the normal path.
        done.store(true, Ordering::Relaxed);

        if let Some(spinner) = spinner {
            spinner.join().ok();
        }

        if let Some(title) = title {
            if set_title {
                set_pane_title(&pane_id, &title);
            }
            title_map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pane_id.clone(), title.clone());
            eprintln!("pane {} -> {}", pane_id, title);
        }

        in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&pane_id);
    });
}

fn spawn_window_title_generation(
    window_id: String,
    pane_titles: String,
    model: Arc<str>,
    in_flight: InFlight,
) {
    thread::spawn(move || {
        let title = call_claude(&model, WINDOW_TITLE_PROMPT, &pane_titles);
        if let Some(title) = title {
            set_window_title(&window_id, &title);
            eprintln!("window {} -> {}", window_id, title);
        }

        in_flight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&window_id);
    });
}

fn main() {
    let args = Args::parse();

    let regen_delay = Duration::from_secs(args.regenerate_delay);
    let poll_interval = Duration::from_secs(args.poll_interval);
    let title_map: TitleMap = Arc::new(Mutex::new(HashMap::new()));
    let in_flight: InFlight = Arc::new(Mutex::new(HashSet::new()));
    let model: Arc<str> = Arc::from(args.model.as_str());

    eprintln!(
        "tmux-ai-titles: starting (model={}, poll={}s, regen_delay={}s, capture={}, hash={})",
        model, args.poll_interval, args.regenerate_delay, args.capture_lines, args.hash_lines
    );

    let mut pane_states: HashMap<String, ChangeTracker> = HashMap::new();
    let mut window_states: HashMap<String, ChangeTracker> = HashMap::new();

    loop {
        let panes = list_panes();

        let active_pane_ids: HashSet<&str> = panes.iter().map(|p| p.pane_id.as_str()).collect();
        pane_states.retain(|id, _| active_pane_ids.contains(id.as_str()));

        let active_window_ids: HashSet<&str> = panes.iter().map(|p| p.window_id.as_str()).collect();
        window_states.retain(|id, _| active_window_ids.contains(id.as_str()));

        // Clean up title_map for removed panes
        title_map
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|id, _| active_pane_ids.contains(id.as_str()));

        let now = Instant::now();

        // --- Pane titles ---
        for pane in &panes {
            // Single capture per pane per cycle: capture the full buffer, then
            // hash just the last N lines for change detection.
            let Some(buffer) = capture_pane(&pane.pane_id, args.capture_lines) else {
                continue;
            };

            let hash = hash_buffer(&buffer, args.hash_lines);

            let state = pane_states
                .entry(pane.pane_id.clone())
                .or_insert_with(|| ChangeTracker::new(hash, now));

            state.update_hash(hash, now);

            if state.should_generate(now, regen_delay) {
                // Atomically check-and-insert to avoid TOCTOU
                let was_inserted = in_flight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(pane.pane_id.clone());

                if !was_inserted {
                    continue;
                }

                spawn_pane_title_generation(
                    pane.pane_id.clone(),
                    buffer,
                    model.clone(),
                    !args.no_pane_titles,
                    title_map.clone(),
                    in_flight.clone(),
                );
                state.mark_generated(now);
            }
        }

        // --- Window titles ---
        if !args.no_window_titles {
            let mut window_panes: HashMap<&str, Vec<(String, bool)>> = HashMap::new();
            let titles_snapshot = title_map.lock().unwrap_or_else(|e| e.into_inner());
            let in_flight_snapshot = in_flight.lock().unwrap_or_else(|e| e.into_inner());

            for pane in &panes {
                let pane_in_flight = in_flight_snapshot.contains(&pane.pane_id);

                // Prefer our internal map, fall back to tmux pane title
                let title = titles_snapshot
                    .get(&pane.pane_id)
                    .cloned()
                    .unwrap_or_else(|| get_pane_title(&pane.pane_id));

                window_panes
                    .entry(pane.window_id.as_str())
                    .or_default()
                    .push((title, pane_in_flight));
            }
            drop(titles_snapshot);
            drop(in_flight_snapshot);

            for (window_id, titles) in &window_panes {
                let combined: String = titles.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(", ");
                let hash = hash_str(&combined);

                let state = window_states
                    .entry(window_id.to_string())
                    .or_insert_with(|| ChangeTracker::new(hash, now));

                state.update_hash(hash, now);

                if state.should_generate(now, regen_delay) {
                    // Skip panes that are currently generating titles
                    let real_titles: Vec<&str> = titles
                        .iter()
                        .filter(|(t, pane_in_flight)| !t.is_empty() && !pane_in_flight)
                        .map(|(t, _)| t.as_str())
                        .collect();

                    if real_titles.is_empty() {
                        continue;
                    }

                    // Atomically check-and-insert to avoid TOCTOU
                    let was_inserted = in_flight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(window_id.to_string());

                    if !was_inserted {
                        continue;
                    }

                    let input = real_titles
                        .iter()
                        .enumerate()
                        .map(|(i, t)| format!("{}. {}", i + 1, t))
                        .collect::<Vec<_>>()
                        .join("\n");

                    in_flight
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(window_id.to_string());

                    spawn_window_title_generation(
                        window_id.to_string(),
                        input,
                        model.clone(),
                        in_flight.clone(),
                    );
                    state.mark_generated(now);
                }
            }
        }

        thread::sleep(poll_interval);
    }
}
