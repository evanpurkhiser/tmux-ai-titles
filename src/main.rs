use clap::{Parser, Subcommand};
use signal_hook::consts::SIGTERM;
use signal_hook::iterator::Signals;
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠚", "⠞", "⠖", "⠦", "⠴", "⠲", "⠳", "⠓"];

const PANE_TITLE_PROMPT: &str = "\
Generate a 4-5 word title for a tmux pane based on the terminal output, running command, \
and working directory provided below. The title should describe the most recent overarching \
task or activity. Weight the END of the output most heavily — that is the most recent \
activity. Earlier history may contain unrelated commands; ignore those unless they fit \
the current theme. Always prioritize terminal output content over the working directory. \
Only fall back to the directory name if the output is truly empty. \
The output may contain TUI formatting (unicode symbols, box-drawing characters) from tools \
like Claude Code, vim, or other terminal apps — look past the formatting to understand \
the task being performed. \
Never use generic titles like \"Shell in X\" when there is meaningful output to describe. \
Never use words like \"idle\", \"awaiting\", or \"waiting\". \
Output ONLY the 4-5 word title. No quotes, no punctuation, no explanation.";

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
    /// Regenerate titles on the next cycle
    Regenerate {
        /// Pane (%N) or window (@N) IDs to regenerate. Omit to regenerate all.
        targets: Vec<String>,
    },
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

    /// Stay in the foreground instead of forking to the background
    #[arg(long)]
    no_bg: bool,
}

fn socket_path() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(dir).join("tmux-ai-titles.sock")
    } else {
        PathBuf::from("/tmp/tmux-ai-titles.sock")
    }
}

/// Wire protocol messages exchanged over the control socket. The daemon
/// receives these; the client encodes them.
enum Request {
    Stop,
    Regenerate(Vec<String>),
    Status,
}

enum ParseError {
    Empty,
    Unknown(String),
}

impl FromStr for Request {
    type Err = ParseError;

    fn from_str(line: &str) -> Result<Self, ParseError> {
        let mut parts = line.split_whitespace();
        let cmd = parts.next().ok_or(ParseError::Empty)?;
        match cmd {
            "stop" => Ok(Request::Stop),
            "status" => Ok(Request::Status),
            "regenerate" => Ok(Request::Regenerate(parts.map(str::to_string).collect())),
            other => Err(ParseError::Unknown(other.to_string())),
        }
    }
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Request::Stop => f.write_str("stop"),
            Request::Status => f.write_str("status"),
            Request::Regenerate(targets) if targets.is_empty() => f.write_str("regenerate"),
            Request::Regenerate(targets) => write!(f, "regenerate {}", targets.join(" ")),
        }
    }
}

/// Fork into the background and detach from the controlling terminal. Returns
/// in the child; the parent exits inside `fork::daemon`. Must be called
/// before any threads are spawned — fork() with live threads is UB.
///
/// Args are `nochdir=true` (stay in cwd) and `noclose=true` (inherit stdio so
/// shell redirections like `start 2>log` keep working).
fn daemonize() {
    if let Err(errno) = fork::daemon(true, true) {
        eprintln!("tmux-ai-titles: failed to daemonize (errno {errno})");
        std::process::exit(1);
    }
}

/// Bind the control socket. If the path exists, probe whether another daemon
/// is listening; unlink and retry only if nothing answers.
fn bind_socket(path: &Path) -> std::io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                Err(e)
            } else {
                std::fs::remove_file(path)?;
                UnixListener::bind(path)
            }
        }
        Err(e) => Err(e),
    }
}

/// Send a request to the daemon and return its response line.
fn send_request(req: &Request) -> std::io::Result<String> {
    let mut stream = UnixStream::connect(socket_path())?;
    stream.write_all(req.to_string().as_bytes())?;
    stream.write_all(b"\n")?;
    stream.shutdown(Shutdown::Write)?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    Ok(resp.trim().to_string())
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

/// Pending regeneration scope. All subsumes Specific — if both are requested
/// before the main loop consumes the request, All wins.
enum RegenerateScope {
    All,
    Specific(HashSet<String>),
}

struct CommandState {
    should_stop: bool,
    pending_regenerate: Option<RegenerateScope>,
}

struct CommandNotifier {
    state: Mutex<CommandState>,
    cvar: Condvar,
}

impl CommandNotifier {
    fn new() -> Self {
        Self {
            state: Mutex::new(CommandState {
                should_stop: false,
                pending_regenerate: None,
            }),
            cvar: Condvar::new(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, CommandState> {
        self.state.lock().unwrap()
    }

    fn notify(&self) {
        self.cvar.notify_one();
    }

    fn request_stop(&self) {
        self.lock().should_stop = true;
        self.notify();
    }

    fn request_regenerate(&self, targets: Vec<String>) {
        let mut state = self.lock();
        state.pending_regenerate = Some(if targets.is_empty() {
            RegenerateScope::All
        } else {
            match state.pending_regenerate.take() {
                Some(RegenerateScope::All) => RegenerateScope::All,
                Some(RegenerateScope::Specific(mut existing)) => {
                    existing.extend(targets);
                    RegenerateScope::Specific(existing)
                }
                None => RegenerateScope::Specific(targets.into_iter().collect()),
            }
        });
        self.notify();
    }

    fn wait_timeout(&self, timeout: Duration) {
        let state = self.lock();
        if !state.should_stop && state.pending_regenerate.is_none() {
            let _ = self.cvar.wait_timeout(state, timeout);
        }
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
    done_tx: mpsc::Sender<String>,
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
                .unwrap()
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
                .unwrap()
                .insert(pane_id.clone(), title.clone());
            eprintln!("pane {} -> {}", pane_id, title);
        }

        done_tx.send(pane_id).ok();
    });
}

fn spawn_window_title_generation(
    window_id: String,
    pane_titles: String,
    model: Arc<str>,
    done_tx: mpsc::Sender<String>,
) {
    thread::spawn(move || {
        let title = call_claude(&model, WINDOW_TITLE_PROMPT, &pane_titles);
        if let Some(title) = title {
            set_window_title(&window_id, &title);
            eprintln!("window {} -> {}", window_id, title);
        }

        done_tx.send(window_id).ok();
    });
}

fn handle_request(req: Request, notifier: &CommandNotifier) -> String {
    match req {
        Request::Stop => {
            notifier.request_stop();
            "ok stopping".into()
        }
        Request::Regenerate(targets) => {
            let n = targets.len();
            notifier.request_regenerate(targets);
            if n == 0 {
                "ok regenerating all".into()
            } else {
                format!("ok regenerating {n} target(s)")
            }
        }
        Request::Status => format!("ok pid={} running", std::process::id()),
    }
}

fn spawn_socket_listener(listener: UnixListener, notifier: Arc<CommandNotifier>) {
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let notifier = notifier.clone();
            thread::spawn(move || {
                let Ok(read_stream) = stream.try_clone() else {
                    return;
                };
                let mut reader = BufReader::new(read_stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    return;
                }
                let response = match line.trim().parse::<Request>() {
                    Ok(req) => handle_request(req, &notifier),
                    Err(ParseError::Empty) => "err empty command".into(),
                    Err(ParseError::Unknown(c)) => format!("err unknown command: {c}"),
                };
                let mut writer = stream;
                let _ = writeln!(writer, "{response}");
            });
        }
    });
}

fn cmd_stop() {
    match send_request(&Request::Stop) {
        Ok(resp) => eprintln!("tmux-ai-titles: {resp}"),
        Err(e) => {
            eprintln!("tmux-ai-titles: daemon is not running ({e})");
            std::process::exit(1);
        }
    }
}

fn cmd_regenerate(targets: Vec<String>) {
    match send_request(&Request::Regenerate(targets)) {
        Ok(resp) => eprintln!("tmux-ai-titles: {resp}"),
        Err(e) => {
            eprintln!("tmux-ai-titles: daemon is not running ({e})");
            std::process::exit(1);
        }
    }
}

fn cmd_status() {
    match send_request(&Request::Status) {
        Ok(resp) => println!("tmux-ai-titles: {resp}"),
        Err(_) => {
            println!("tmux-ai-titles: not running");
            std::process::exit(1);
        }
    }
}

fn cmd_start(args: StartArgs) {
    let sock_path = socket_path();

    let listener = match bind_socket(&sock_path) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            eprintln!(
                "tmux-ai-titles: already running (socket {} is in use)",
                sock_path.display()
            );
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!(
                "tmux-ai-titles: failed to bind socket {}: {}",
                sock_path.display(),
                e
            );
            std::process::exit(1);
        }
    };

    // Fork BEFORE spawning any threads — fork() with live threads is UB.
    // The listening socket fd is inherited by the child across fork.
    if !args.no_bg {
        daemonize();
    }

    let regen_delay = Duration::from_secs(args.regenerate_delay);
    let poll_interval = Duration::from_secs(args.poll_interval);
    let title_map: TitleMap = Arc::new(Mutex::new(HashMap::new()));
    let mut in_flight: HashSet<String> = HashSet::new();
    let (done_tx, done_rx) = mpsc::channel::<String>();
    let model: Arc<str> = Arc::from(args.model.as_str());

    let notifier = Arc::new(CommandNotifier::new());

    spawn_socket_listener(listener, notifier.clone());

    // Keep SIGTERM as a fallback so `kill` still shuts the daemon down cleanly.
    {
        let notifier = notifier.clone();
        let mut signals = Signals::new([SIGTERM]).expect("failed to register signal handlers");
        thread::spawn(move || {
            for sig in signals.forever() {
                if sig == SIGTERM {
                    notifier.request_stop();
                }
            }
        });
    }

    eprintln!(
        "tmux-ai-titles: starting (pid={}, socket={}, model={}, poll={}s, regen_delay={}s, capture={}, hash={})",
        std::process::id(),
        sock_path.display(),
        model,
        args.poll_interval,
        args.regenerate_delay,
        args.capture_lines,
        args.hash_lines
    );

    let mut pane_states: HashMap<String, ChangeTracker> = HashMap::new();
    let mut window_states: HashMap<String, ChangeTracker> = HashMap::new();

    loop {
        // Reap completed workers before deciding what to spawn this cycle.
        while let Ok(id) = done_rx.try_recv() {
            in_flight.remove(&id);
        }

        let regenerate_scope = {
            let mut state = notifier.lock();

            if state.should_stop {
                eprintln!("tmux-ai-titles: stop requested, shutting down");
                let _ = std::fs::remove_file(&sock_path);
                break;
            }

            state.pending_regenerate.take()
        };

        if let Some(scope) = regenerate_scope {
            match &scope {
                RegenerateScope::All => {
                    eprintln!("tmux-ai-titles: regenerate requested for all panes/windows");
                    for s in pane_states.values_mut() {
                        s.has_generated = false;
                    }
                    for s in window_states.values_mut() {
                        s.has_generated = false;
                    }
                }
                RegenerateScope::Specific(ids) => {
                    eprintln!(
                        "tmux-ai-titles: regenerate requested for {}",
                        ids.iter().map(String::as_str).collect::<Vec<_>>().join(" ")
                    );
                    for (id, s) in pane_states.iter_mut() {
                        if ids.contains(id) {
                            s.has_generated = false;
                        }
                    }
                    for (id, s) in window_states.iter_mut() {
                        if ids.contains(id) {
                            s.has_generated = false;
                        }
                    }
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
            .unwrap()
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
                if !in_flight.insert(pane.pane_id.clone()) {
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
                    done_tx.clone(),
                );
                state.mark_generated(now);
            }
        }

        // --- Window titles ---
        if !args.no_window_titles {
            let mut window_panes: HashMap<&str, Vec<(String, String, bool)>> = HashMap::new();
            let titles_snapshot = title_map.lock().unwrap();

            for pane in &panes {
                let pane_in_flight = in_flight.contains(&pane.pane_id);

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

                    if !in_flight.insert(window_id.to_string()) {
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
                        done_tx.clone(),
                    );
                    state.mark_generated(now);
                }
            }
        }

        // Wait for poll interval or until a signal/command wakes us
        notifier.wait_timeout(poll_interval);
    }
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Start(args) => cmd_start(args),
        Commands::Stop => cmd_stop(),
        Commands::Regenerate { targets } => cmd_regenerate(targets),
        Commands::Status => cmd_status(),
    }
}
