use clap::{Parser, Subcommand};
use signal_hook::consts::{SIGHUP, SIGTERM};
use signal_hook::iterator::Signals;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠚", "⠞", "⠖", "⠦", "⠴", "⠲", "⠳", "⠓"];

const PANE_TITLE_PROMPT: &str = "\
Generate a 4-5 word title for a tmux pane. Focus on the specific task or activity \
visible in the output — e.g. \"Editing Rust build config\" not \"Terminal Session\". \
Use the working directory and previous title for context when the output alone is ambiguous. \
If the terminal just shows a shell prompt with no meaningful recent output, title it \
based on the working directory — e.g. \"Shell in tmux-ai-titles\". Never use words like \
\"idle\", \"awaiting\", or \"waiting\" in the title. \
Output ONLY the title. No quotes, no punctuation, no explanation.";

const WINDOW_TITLE_PROMPT: &str = "\
Generate a 1-2 word tmux window title from these pane descriptions. \
The title should capture the project or theme — e.g. a window with panes doing \
Rust builds, editing source, and running tests might be titled \"Rust Dev\". \
Each entry includes the pane title and its working directory for context. \
Output ONLY the title. No quotes, no punctuation, no explanation.";

/// AI-powered title generation for tmux panes and windows
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the title generation daemon
    Start(StartArgs),
    /// Stop the running daemon
    Stop,
    /// Regenerate all titles on the next cycle
    Regenerate,
    /// Check if the daemon is running
    Status,
}

#[derive(Parser)]
struct StartArgs {
    /// Seconds to wait after content changes before regenerating
    #[arg(long, default_value_t = 300)]
    regenerate_delay: u64,

    /// Lines of buffer to capture for title generation
    #[arg(long, default_value_t = 250)]
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

    /// Run in the foreground (don't check PID file)
    #[arg(long)]
    foreground: bool,
}

fn pid_file_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("tmux-ai-titles.pid")
    } else {
        PathBuf::from("/tmp/tmux-ai-titles.pid")
    }
}

fn read_pid() -> Option<u32> {
    let path = pid_file_path();
    let contents = std::fs::read_to_string(&path).ok()?;
    contents.trim().parse().ok()
}

fn process_is_running(pid: u32) -> bool {
    // kill -0 checks if process exists without actually sending a signal
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn write_pid_file() {
    let path = pid_file_path();
    if let Ok(mut file) = std::fs::File::create(&path) {
        let _ = write!(file, "{}", std::process::id());
    }
}

fn remove_pid_file() {
    let _ = std::fs::remove_file(pid_file_path());
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
    cwd: String,
    command: String,
}

/// Shared map of pane_id -> generated title, used for window title generation
type TitleMap = Arc<Mutex<HashMap<String, String>>>;

/// Set of pane/window IDs currently being processed by a background thread
type InFlight = Arc<Mutex<HashSet<String>>>;

struct SignalState {
    should_stop: bool,
    should_regenerate: bool,
}

struct SignalNotifier {
    state: Mutex<SignalState>,
    cvar: Condvar,
}

impl SignalNotifier {
    fn new() -> Self {
        Self {
            state: Mutex::new(SignalState {
                should_stop: false,
                should_regenerate: false,
            }),
            cvar: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SignalState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn notify(&self) {
        self.cvar.notify_one();
    }

    fn wait_timeout(&self, timeout: Duration) {
        let state = self.lock();
        let _ = self.cvar.wait_timeout(state, timeout);
    }
}

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
        .args([
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{window_id}\t#{pane_current_command}\t#{pane_current_path}",
        ])
        .output()
        .ok();

    let Some(output) = output else { return vec![] };
    let stdout = String::from_utf8_lossy(&output.stdout);

    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(4, '\t');
            let pane_id = parts.next()?.to_string();
            let window_id = parts.next()?.to_string();
            let command = parts.next().unwrap_or("").to_string();
            let cwd = parts.next().unwrap_or("").to_string();
            Some(PaneInfo {
                pane_id,
                window_id,
                cwd,
                command,
            })
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
    cwd: String,
    command: String,
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

        let mut context = String::new();
        if !current_title.is_empty() {
            context.push_str(&format!(
                "Previous title (for context only, may be outdated): {current_title}\n"
            ));
        }
        if !command.is_empty() {
            context.push_str(&format!("Running command: {command}\n"));
        }
        if !cwd.is_empty() {
            context.push_str(&format!("Working directory: {cwd}\n"));
        }
        let input = if context.is_empty() {
            format!("```\n{buffer}\n```")
        } else {
            format!("{context}\n```\n{buffer}\n```")
        };
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

fn cmd_stop() {
    let Some(pid) = read_pid() else {
        eprintln!("tmux-ai-titles: no PID file found, daemon is not running");
        std::process::exit(1);
    };

    if !process_is_running(pid) {
        eprintln!(
            "tmux-ai-titles: stale PID file (process {} not running), cleaning up",
            pid
        );
        remove_pid_file();
        std::process::exit(1);
    }

    // Send SIGTERM
    let status = Command::new("kill").args([&pid.to_string()]).status();

    match status {
        Ok(s) if s.success() => eprintln!("tmux-ai-titles: sent SIGTERM to process {}", pid),
        _ => {
            eprintln!("tmux-ai-titles: failed to send SIGTERM to process {}", pid);
            std::process::exit(1);
        }
    }
}

fn cmd_regenerate() {
    let Some(pid) = read_pid() else {
        eprintln!("tmux-ai-titles: no PID file found, daemon is not running");
        std::process::exit(1);
    };

    if !process_is_running(pid) {
        eprintln!(
            "tmux-ai-titles: stale PID file (process {} not running), cleaning up",
            pid
        );
        remove_pid_file();
        std::process::exit(1);
    }

    let status = Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status();

    match status {
        Ok(s) if s.success() => eprintln!(
            "tmux-ai-titles: sent SIGHUP to process {} (will regenerate all titles)",
            pid
        ),
        _ => {
            eprintln!("tmux-ai-titles: failed to send SIGHUP to process {}", pid);
            std::process::exit(1);
        }
    }
}

fn cmd_status() {
    let Some(pid) = read_pid() else {
        println!("tmux-ai-titles: not running (no PID file)");
        std::process::exit(1);
    };

    if process_is_running(pid) {
        println!("tmux-ai-titles: running (PID {})", pid);
    } else {
        println!(
            "tmux-ai-titles: not running (stale PID file for process {})",
            pid
        );
        remove_pid_file();
        std::process::exit(1);
    }
}

fn cmd_start(args: StartArgs) {
    if !args.foreground {
        // Check if already running
        if let Some(pid) = read_pid() {
            if process_is_running(pid) {
                eprintln!("tmux-ai-titles: already running (PID {})", pid);
                std::process::exit(1);
            }
            // Stale PID file, clean up
            remove_pid_file();
        }

        write_pid_file();
    }

    let regen_delay = Duration::from_secs(args.regenerate_delay);
    let poll_interval = Duration::from_secs(args.poll_interval);
    let title_map: TitleMap = Arc::new(Mutex::new(HashMap::new()));
    let in_flight: InFlight = Arc::new(Mutex::new(HashSet::new()));
    let model: Arc<str> = Arc::from(args.model.as_str());

    let signals_notifier = Arc::new(SignalNotifier::new());

    let mut signals = Signals::new([SIGTERM, SIGHUP]).expect("failed to register signal handlers");

    {
        let notifier = signals_notifier.clone();
        thread::spawn(move || {
            for sig in signals.forever() {
                let mut state = notifier.lock();
                match sig {
                    SIGTERM => state.should_stop = true,
                    SIGHUP => state.should_regenerate = true,
                    _ => continue,
                }
                drop(state);
                notifier.notify();
            }
        });
    }

    eprintln!(
        "tmux-ai-titles: starting (pid={}, model={}, poll={}s, regen_delay={}s, capture={}, hash={})",
        std::process::id(),
        model,
        args.poll_interval,
        args.regenerate_delay,
        args.capture_lines,
        args.hash_lines
    );

    let mut pane_states: HashMap<String, ChangeTracker> = HashMap::new();
    let mut window_states: HashMap<String, ChangeTracker> = HashMap::new();

    loop {
        {
            let mut state = signals_notifier.lock();

            if state.should_stop {
                eprintln!("tmux-ai-titles: received SIGTERM, shutting down");
                remove_pid_file();
                break;
            }

            if state.should_regenerate {
                eprintln!("tmux-ai-titles: received SIGHUP, clearing generated flags");
                state.should_regenerate = false;
                for s in pane_states.values_mut() {
                    s.has_generated = false;
                }
                for s in window_states.values_mut() {
                    s.has_generated = false;
                }
            }
        }

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
                    pane.cwd.clone(),
                    pane.command.clone(),
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
            let mut window_panes: HashMap<&str, Vec<(String, String, bool)>> = HashMap::new();
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
                    .push((title, pane.cwd.clone(), pane_in_flight));
            }
            drop(titles_snapshot);
            drop(in_flight_snapshot);

            for (window_id, titles) in &window_panes {
                let combined: String = titles
                    .iter()
                    .map(|(t, c, _)| format!("{t} {c}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let hash = hash_str(&combined);

                let state = window_states
                    .entry(window_id.to_string())
                    .or_insert_with(|| ChangeTracker::new(hash, now));

                state.update_hash(hash, now);

                if state.should_generate(now, regen_delay) {
                    // Skip panes that are currently generating titles
                    let real_titles: Vec<(&str, &str)> = titles
                        .iter()
                        .filter(|(t, _, in_flight)| !t.is_empty() && !in_flight)
                        .map(|(t, c, _)| (t.as_str(), c.as_str()))
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
                        .map(|(i, (t, c))| format!("{}. {} (cwd: {})", i + 1, t, c))
                        .collect::<Vec<_>>()
                        .join("\n");

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

        // Wait for poll interval or until a signal wakes us
        signals_notifier.wait_timeout(poll_interval);
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start(args) => cmd_start(args),
        Commands::Stop => cmd_stop(),
        Commands::Regenerate => cmd_regenerate(),
        Commands::Status => cmd_status(),
    }
}
