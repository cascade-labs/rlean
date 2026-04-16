/// `rlean research <project>` — project-scoped PyO3 research kernel
///
/// Manages a persistent research session per project using a background Rust
/// daemon (`rlean __research-daemon`) that embeds Python via PyO3.
/// No Jupyter / ipykernel required.
///
/// Usage:
///   rlean research <project>                        # start kernel
///   rlean research <project> add-cell "code"        # append cell, execute, save outputs
///   rlean research <project> add-cell --at N "code" # insert at index N, execute
///   rlean research <project> upsert-cell N "code"   # replace cell N source, re-execute
///   rlean research <project> run-cell N             # execute cell N, update outputs
///   rlean research <project> run-all                # execute all code cells in order
///   rlean research <project> cells                  # list cells with index + preview
///   rlean research <project> get-cell N             # print full source of cell N
///   rlean research <project> delete-cell N          # remove cell N
///   rlean research <project> clear-outputs          # strip all outputs (keep source)
///   rlean research <project> vars                   # list kernel namespace
///   rlean research <project> shutdown               # stop kernel
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::project::research_notebook;
use crate::research_daemon::session_dir;

// ── CLI types ─────────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct ResearchArgs {
    /// Project directory (must contain config.json)
    pub project: PathBuf,

    #[command(subcommand)]
    pub command: Option<ResearchCommand>,
}

#[derive(clap::Subcommand)]
pub enum ResearchCommand {
    /// Append a new code cell (or insert at --at N), execute it, save outputs
    #[command(name = "add-cell")]
    AddCell {
        /// Insert at this index instead of appending to the end
        #[arg(long, value_name = "N")]
        at: Option<usize>,
        /// Python source code for the new cell
        code: String,
    },
    /// Replace the source of cell N with new code, re-execute, update outputs
    #[command(name = "upsert-cell")]
    UpsertCell {
        /// Cell index to replace (0-indexed)
        index: usize,
        /// New Python source code
        code: String,
    },
    /// Execute cell N in the kernel and update its outputs
    #[command(name = "run-cell")]
    RunCell { index: usize },
    /// Execute all code cells in order (restores kernel state after restart)
    #[command(name = "run-all")]
    RunAll,
    /// List all notebook cells with index, type, output count, and first-line preview
    Cells,
    /// Print the full source of cell N (0-indexed)
    #[command(name = "get-cell")]
    GetCell { index: usize },
    /// Delete cell N from the notebook (0-indexed)
    #[command(name = "delete-cell")]
    DeleteCell { index: usize },
    /// Strip all outputs from every cell (keeps source intact)
    #[command(name = "clear-outputs")]
    ClearOutputs,
    /// List variables in the kernel namespace
    Vars,
    /// Shutdown the project kernel
    Shutdown,
}

// ── Wire protocol (client side) ───────────────────────────────────────────────

#[derive(Serialize)]
struct Req<'a> {
    op:   &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    code: &'a str,
}

#[derive(Deserialize, Default)]
struct Resp {
    status:  String,
    #[serde(default)] stdout:  String,
    #[serde(default)] stderr:  String,
    #[serde(default)] figures: Vec<String>,
    #[serde(default)] message: String,
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_research(args: ResearchArgs) -> Result<()> {
    let project_dir = args.project.canonicalize().with_context(|| {
        format!("Project directory not found: {}", args.project.display())
    })?;

    if !project_dir.join("config.json").exists() {
        bail!(
            "'{}' is not a valid project (missing config.json).\n\
             Create one first: rlean create-project <name>",
            project_dir.display()
        );
    }

    // Ensure research.ipynb exists.
    let notebook = project_dir.join("research.ipynb");
    if !notebook.exists() {
        let name = project_dir.file_name().unwrap_or_default().to_string_lossy();
        std::fs::write(&notebook, research_notebook(&name))
            .context("Failed to create research.ipynb")?;
        println!("Created research.ipynb in '{}'", project_dir.display());
    }

    let session = session_name(&project_dir);
    let sdir    = session_dir(&session)?;
    let sock    = sdir.join("server.sock");

    match args.command {
        None =>
            kernel_start(&project_dir, &session, &sock),
        Some(ResearchCommand::AddCell { at, code }) =>
            cmd_add_cell(&session, &sock, &notebook, at, &code),
        Some(ResearchCommand::UpsertCell { index, code }) =>
            cmd_upsert_cell(&session, &sock, &notebook, index, &code),
        Some(ResearchCommand::RunCell { index }) =>
            cmd_run_cell(&session, &sock, &notebook, index),
        Some(ResearchCommand::RunAll) =>
            cmd_run_all(&session, &sock, &notebook),
        Some(ResearchCommand::Cells) =>
            cmd_cells(&notebook),
        Some(ResearchCommand::GetCell { index }) =>
            cmd_get_cell(&notebook, index),
        Some(ResearchCommand::DeleteCell { index }) =>
            cmd_delete_cell(&notebook, index),
        Some(ResearchCommand::ClearOutputs) =>
            cmd_clear_outputs(&notebook),
        Some(ResearchCommand::Vars) =>
            kernel_vars(&sock),
        Some(ResearchCommand::Shutdown) =>
            kernel_shutdown(&sock),
    }
}

// ── Kernel lifecycle ──────────────────────────────────────────────────────────

fn kernel_start(project_dir: &Path, session: &str, sock: &Path) -> Result<()> {
    if daemon_is_alive(sock) {
        println!("Research kernel already running.  Session: {session}");
        return Ok(());
    }

    let data_folder = find_data_folder(project_dir);

    println!("Starting research kernel for session '{session}'...");
    spawn_daemon(session, project_dir, data_folder.as_deref())?;
    wait_for_daemon(sock, 60)?;

    println!("Kernel ready.  Session: {session}");
    println!("Run code:  rlean research {} exec \"<python>\"", project_dir.display());
    Ok(())
}

/// Insert a new cell at `at` (or append), execute it, store outputs.
fn cmd_add_cell(
    session:  &str,
    sock:     &Path,
    notebook: &Path,
    at:       Option<usize>,
    code:     &str,
) -> Result<()> {
    ensure_alive(sock, session)?;

    let resp = send_request(sock, "exec", code)?;
    print_response(&resp);

    let fig_paths = save_figures(session, &resp.figures)?;
    for p in &fig_paths { let _ = std::process::Command::new("open").arg(p).spawn(); }

    insert_notebook_cell(notebook, at, code, &resp.stdout, &resp.figures)?;

    let idx = at.map(|n| n.to_string()).unwrap_or_else(|| "end".to_string());
    println!("Cell added at [{idx}].");
    Ok(())
}

/// Replace cell N's source with new code, re-execute, update its outputs.
fn cmd_upsert_cell(
    session:  &str,
    sock:     &Path,
    notebook: &Path,
    index:    usize,
    code:     &str,
) -> Result<()> {
    ensure_alive(sock, session)?;

    // Validate index before hitting the kernel.
    {
        let cells = read_cells(notebook)?;
        if index >= cells.len() {
            bail!("Cell index {index} out of range (notebook has {} cells)", cells.len());
        }
    }

    let resp = send_request(sock, "exec", code)?;
    print_response(&resp);

    let fig_paths = save_figures(session, &resp.figures)?;
    for p in &fig_paths { let _ = std::process::Command::new("open").arg(p).spawn(); }

    replace_cell_source_and_outputs(notebook, index, code, &resp.stdout, &resp.figures)?;
    println!("Cell [{index}] updated.");
    Ok(())
}

fn kernel_vars(sock: &Path) -> Result<()> {
    let resp = send_request(sock, "vars", "")?;
    print_response(&resp);
    Ok(())
}

fn kernel_shutdown(sock: &Path) -> Result<()> {
    if !sock.exists() {
        println!("No kernel running.");
        return Ok(());
    }
    let resp = send_request(sock, "shutdown", "")?;
    print_response(&resp);
    Ok(())
}

// ── Notebook commands ─────────────────────────────────────────────────────────

/// List all cells: index, type, and first ~60 chars of source.
fn cmd_cells(notebook: &Path) -> Result<()> {
    let cells = read_cells(notebook)?;
    if cells.is_empty() {
        println!("Notebook has no cells.");
        return Ok(());
    }
    let total = cells.len();
    println!("{total} cell(s) in {}:", notebook.display());
    println!();
    for (i, cell) in cells.iter().enumerate() {
        let kind    = &cell.cell_type;
        let source  = cell.source.join("");
        let preview = first_line_preview(&source, 70);
        let outputs = cell.outputs.len();
        let out_str = if outputs > 0 { format!("  [{outputs} output(s)]") } else { String::new() };
        println!("  [{i:>3}] {kind:<4}  {preview}{out_str}");
    }
    Ok(())
}

/// Print the full source of cell N.
fn cmd_get_cell(notebook: &Path, index: usize) -> Result<()> {
    let cells = read_cells(notebook)?;
    let cell  = cells.get(index).with_context(|| format!(
        "Cell index {index} out of range (notebook has {} cells)", cells.len()
    ))?;
    let source = cell.source.join("");
    println!("── Cell {index} ({}) ────────────────────────────────", cell.cell_type);
    print!("{source}");
    if !source.ends_with('\n') { println!(); }
    Ok(())
}

/// Re-execute cell N in the kernel, replace its outputs in the notebook.
fn cmd_run_cell(session: &str, sock: &Path, notebook: &Path, index: usize) -> Result<()> {
    ensure_alive(sock, session)?;

    let cells  = read_cells(notebook)?;
    let cell   = cells.get(index).with_context(|| format!(
        "Cell index {index} out of range (notebook has {} cells)", cells.len()
    ))?;

    if cell.cell_type != "code" {
        bail!("Cell {index} is a markdown cell — nothing to execute");
    }

    let code = cell.source.join("");
    println!("Running cell [{index}]...");

    let resp = send_request(sock, "exec", &code)?;
    print_response(&resp);

    let fig_paths = save_figures(session, &resp.figures)?;
    for p in &fig_paths {
        let _ = std::process::Command::new("open").arg(p).spawn();
    }

    // Replace outputs for this cell in the notebook.
    update_cell_outputs(notebook, index, &resp.stdout, &resp.figures)?;
    Ok(())
}

/// Delete cell N from the notebook.
fn cmd_delete_cell(notebook: &Path, index: usize) -> Result<()> {
    let text = read_notebook_raw(notebook)?;
    let mut nb: serde_json::Value = serde_json::from_str(&text)?;

    let cells = nb["cells"].as_array_mut()
        .context("notebook missing cells array")?;

    if index >= cells.len() {
        bail!("Cell index {index} out of range (notebook has {} cells)", cells.len());
    }

    let removed = cells.remove(index);
    let preview = cell_source_preview(&removed, 60);
    write_notebook(notebook, &nb)?;

    println!("Deleted cell [{index}]: {preview}");
    Ok(())
}

/// Strip all outputs from every cell (keeps source intact).
fn cmd_clear_outputs(notebook: &Path) -> Result<()> {
    let text = read_notebook_raw(notebook)?;
    let mut nb: serde_json::Value = serde_json::from_str(&text)?;

    let mut cleared = 0usize;
    if let Some(cells) = nb["cells"].as_array_mut() {
        for cell in cells.iter_mut() {
            if cell["cell_type"].as_str() == Some("code") {
                cell["outputs"]         = serde_json::json!([]);
                cell["execution_count"] = serde_json::Value::Null;
                cleared += 1;
            }
        }
    }

    write_notebook(notebook, &nb)?;
    println!("Cleared outputs from {cleared} code cell(s).");
    Ok(())
}

/// Execute all code cells in notebook order, updating each cell's outputs.
fn cmd_run_all(session: &str, sock: &Path, notebook: &Path) -> Result<()> {
    ensure_alive(sock, session)?;

    let cells: Vec<_> = read_cells(notebook)?
        .into_iter()
        .enumerate()
        .filter(|(_, c)| c.cell_type == "code")
        .collect();

    if cells.is_empty() {
        println!("No code cells to run.");
        return Ok(());
    }

    println!("Running {} code cell(s)...", cells.len());
    for (i, cell) in &cells {
        let code = cell.source.join("");
        if code.trim().is_empty() { continue; }

        let preview = first_line_preview(&code, 50);
        println!("  [{i}] {preview}");

        let resp = send_request(sock, "exec", &code)?;
        if !resp.stdout.is_empty() { print!("{}", resp.stdout); }
        if !resp.stderr.is_empty() { eprint!("{}", resp.stderr); }

        let fig_paths = save_figures(session, &resp.figures)?;
        for p in &fig_paths { let _ = std::process::Command::new("open").arg(p).spawn(); }

        update_cell_outputs(notebook, *i, &resp.stdout, &resp.figures)?;
    }

    println!("Done.");
    Ok(())
}

// ── Socket client ─────────────────────────────────────────────────────────────

fn send_request(sock: &Path, op: &str, code: &str) -> Result<Resp> {
    let mut stream = UnixStream::connect(sock)
        .with_context(|| format!("Cannot connect to kernel: {}", sock.display()))?;

    let req  = Req { op, code };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;

    let mut reader    = BufReader::new(stream);
    let mut resp_line = String::new();
    reader.read_line(&mut resp_line)?;

    serde_json::from_str(resp_line.trim()).context("Failed to parse kernel response")
}

// ── Daemon lifecycle ──────────────────────────────────────────────────────────

fn daemon_is_alive(sock: &Path) -> bool {
    sock.exists() && UnixStream::connect(sock).is_ok()
}

fn ensure_alive(sock: &Path, session: &str) -> Result<()> {
    if !daemon_is_alive(sock) {
        bail!(
            "Research kernel for session '{session}' is not running.\n\
             Start it first: rlean research <project>"
        );
    }
    Ok(())
}

fn spawn_daemon(session: &str, project: &Path, data_folder: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe().context("Cannot determine current exe path")?;

    let mut cmd = std::process::Command::new(&exe);
    cmd.args(["__research-daemon", "--session", session]);
    cmd.args(["--project", &project.to_string_lossy()]);
    if let Some(df) = data_folder {
        cmd.args(["--data-folder", &df.to_string_lossy()]);
    }
    cmd.stdin(std::process::Stdio::null())
       .stdout(std::process::Stdio::null())
       .stderr(std::process::Stdio::null());

    cmd.spawn().context("Failed to spawn research daemon")?;
    Ok(())
}

fn wait_for_daemon(sock: &Path, timeout_secs: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    while Instant::now() < deadline {
        if daemon_is_alive(sock) {
            if let Ok(r) = send_request(sock, "ping", "") {
                if r.status == "pong" { return Ok(()); }
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    bail!("Research kernel did not become ready within {timeout_secs}s")
}

// ── Output helpers ────────────────────────────────────────────────────────────

fn print_response(resp: &Resp) {
    if !resp.stdout.is_empty()  { print!("{}", resp.stdout); }
    if !resp.stderr.is_empty()  { eprint!("{}", resp.stderr); }
    if !resp.message.is_empty() { eprintln!("Kernel error: {}", resp.message); }
    if !resp.figures.is_empty() {
        println!("[{} figure(s) generated]", resp.figures.len());
    }
}

fn session_name(project_dir: &Path) -> String {
    project_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Walk up from `project_dir` looking for `rlean.json`; return the resolved
/// absolute path of the configured `data-folder`.
fn find_data_folder(project_dir: &Path) -> Option<PathBuf> {
    let mut dir = project_dir.to_path_buf();
    loop {
        let cfg = dir.join("rlean.json");
        if cfg.exists() {
            if let Ok(text) = std::fs::read_to_string(&cfg) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(df) = v["data-folder"].as_str() {
                        let candidate = dir.join(df);
                        if candidate.is_dir() {
                            return Some(candidate.canonicalize().unwrap_or(candidate));
                        }
                    }
                }
            }
        }
        match dir.parent() {
            Some(p) if p != dir => dir = p.to_path_buf(),
            _                   => break,
        }
    }
    None
}

/// Decode base64 PNG figures and save to the session plots directory.
fn save_figures(session: &str, figures: &[String]) -> Result<Vec<PathBuf>> {
    if figures.is_empty() { return Ok(vec![]); }

    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;

    let home      = std::env::var("HOME").map(PathBuf::from).context("HOME not set")?;
    let plots_dir = home
        .join(".lean-research")
        .join("sessions")
        .join(session)
        .join("plots");
    std::fs::create_dir_all(&plots_dir)?;

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();

    let mut paths = Vec::new();
    for (i, b64) in figures.iter().enumerate() {
        let bytes = engine.decode(b64).context("Failed to decode figure PNG")?;
        let path  = plots_dir.join(format!("{ts}_{i}.png"));
        std::fs::write(&path, &bytes)?;
        println!("  Saved: {}", path.display());
        paths.push(path);
    }
    Ok(paths)
}

// ── Notebook I/O ──────────────────────────────────────────────────────────────

/// Minimal cell representation for reading the notebook.
struct Cell {
    cell_type: String,
    source:    Vec<String>,
    outputs:   Vec<serde_json::Value>,
}

fn read_notebook_raw(notebook: &Path) -> Result<String> {
    std::fs::read_to_string(notebook)
        .with_context(|| format!("Cannot read {}", notebook.display()))
}

fn read_cells(notebook: &Path) -> Result<Vec<Cell>> {
    let text = read_notebook_raw(notebook)?;
    let nb: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("Invalid JSON in {}", notebook.display()))?;

    let mut out = Vec::new();
    if let Some(arr) = nb["cells"].as_array() {
        for cell in arr {
            let cell_type = cell["cell_type"].as_str().unwrap_or("code").to_string();
            let source = match cell["source"].as_array() {
                Some(lines) => lines.iter()
                    .filter_map(|l| l.as_str())
                    .map(|s| s.to_string())
                    .collect(),
                None => vec![cell["source"].as_str().unwrap_or("").to_string()],
            };
            let outputs = cell["outputs"].as_array()
                .cloned()
                .unwrap_or_default();
            out.push(Cell { cell_type, source, outputs });
        }
    }
    Ok(out)
}

fn write_notebook(notebook: &Path, nb: &serde_json::Value) -> Result<()> {
    std::fs::write(notebook, serde_json::to_string_pretty(nb)?)
        .with_context(|| format!("Cannot write {}", notebook.display()))
}

/// Insert a new code cell at `at` (append if None) with outputs.
fn insert_notebook_cell(
    notebook: &Path,
    at:       Option<usize>,
    code:     &str,
    stdout:   &str,
    figures:  &[String],
) -> Result<()> {
    let text = read_notebook_raw(notebook)?;
    let mut nb: serde_json::Value = serde_json::from_str(&text)?;

    let cell = make_cell(code, stdout, figures);

    if let Some(cells) = nb["cells"].as_array_mut() {
        match at {
            None    => cells.push(cell),
            Some(i) => {
                let i = i.min(cells.len());
                cells.insert(i, cell);
            }
        }
    }
    write_notebook(notebook, &nb)
}

/// Replace cell N's source and outputs entirely.
fn replace_cell_source_and_outputs(
    notebook: &Path,
    index:    usize,
    code:     &str,
    stdout:   &str,
    figures:  &[String],
) -> Result<()> {
    let text = read_notebook_raw(notebook)?;
    let mut nb: serde_json::Value = serde_json::from_str(&text)?;

    let cells = nb["cells"].as_array_mut()
        .context("notebook missing cells array")?;
    let cell = cells.get_mut(index)
        .with_context(|| format!("Cell {index} not found"))?;

    let source: Vec<serde_json::Value> = code.lines().enumerate()
        .map(|(i, l)| serde_json::Value::String(
            if i == 0 { l.to_string() } else { format!("\n{l}") }
        ))
        .collect();

    cell["source"]          = serde_json::Value::Array(source);
    cell["outputs"]         = serde_json::Value::Array(build_outputs(stdout, figures));
    cell["execution_count"] = serde_json::Value::Null;

    write_notebook(notebook, &nb)
}

/// Replace the outputs of cell at `index` in the notebook.
fn update_cell_outputs(
    notebook: &Path,
    index:    usize,
    stdout:   &str,
    figures:  &[String],
) -> Result<()> {
    let text = read_notebook_raw(notebook)?;
    let mut nb: serde_json::Value = serde_json::from_str(&text)?;

    let cells = nb["cells"].as_array_mut()
        .context("notebook missing cells array")?;
    let cell = cells.get_mut(index)
        .with_context(|| format!("Cell {index} not found"))?;

    cell["outputs"]         = serde_json::Value::Array(build_outputs(stdout, figures));
    cell["execution_count"] = serde_json::Value::Null;

    write_notebook(notebook, &nb)
}

fn make_cell(code: &str, stdout: &str, figures: &[String]) -> serde_json::Value {
    let source: Vec<serde_json::Value> = code.lines().enumerate()
        .map(|(i, l)| serde_json::Value::String(
            if i == 0 { l.to_string() } else { format!("\n{l}") }
        ))
        .collect();
    serde_json::json!({
        "cell_type":       "code",
        "execution_count": null,
        "metadata":        {},
        "outputs":         build_outputs(stdout, figures),
        "source":          source,
    })
}

fn build_outputs(stdout: &str, figures: &[String]) -> Vec<serde_json::Value> {
    let mut outputs = Vec::new();
    if !stdout.is_empty() {
        outputs.push(serde_json::json!({
            "output_type": "stream",
            "name":        "stdout",
            "text":        stdout,
        }));
    }
    for fig_b64 in figures {
        outputs.push(serde_json::json!({
            "output_type": "display_data",
            "data": { "image/png": fig_b64, "text/plain": "<Figure>" },
            "metadata": {},
        }));
    }
    outputs
}

// ── String helpers ────────────────────────────────────────────────────────────

/// First non-empty line of `src`, truncated to `max_chars`.
fn first_line_preview(src: &str, max_chars: usize) -> String {
    let line = src.lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    if line.len() <= max_chars {
        line.to_string()
    } else {
        format!("{}…", &line[..max_chars])
    }
}

fn cell_source_preview(cell: &serde_json::Value, max_chars: usize) -> String {
    let src = match cell["source"].as_array() {
        Some(lines) => lines.iter()
            .filter_map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join(""),
        None => cell["source"].as_str().unwrap_or("").to_string(),
    };
    first_line_preview(&src, max_chars)
}
