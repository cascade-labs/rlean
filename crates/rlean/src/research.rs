/// `rlean research <project>` — project-scoped Python kernel
///
/// Manages a persistent ipykernel session per project.
/// No browser UI. Agentic: send code, get output, cells persisted to research.ipynb.
///
/// Requires:  pip install jupyter_client ipykernel
///
/// Usage:
///   rlean research <project>                  # start kernel (loads notebook cells)
///   rlean research <project> exec "code"      # run code, append cell to notebook
///   rlean research <project> exec-file f.py   # run file, append cell to notebook
///   rlean research <project> vars             # list kernel namespace
///   rlean research <project> shutdown         # stop kernel
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::project::research_notebook;

// ── Embedded lean_research Python package ────────────────────────────────────

const PY_INIT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/lean-python/python/lean_research/__init__.py"
));
const PY_SESSION: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/lean-python/python/lean_research/session.py"
));
const PY_CLI: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/lean-python/python/lean_research/cli.py"
));
const PY_KERNEL_INIT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/lean-python/python/lean_research/kernel/__init__.py"
));
const PY_KERNEL_STARTUP: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/lean-python/python/lean_research/kernel/startup.py"
));

// ── CLI types ─────────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct ResearchArgs {
    /// Project directory (must contain config.json and research.ipynb)
    pub project: PathBuf,

    #[command(subcommand)]
    pub command: Option<ResearchCommand>,
}

#[derive(clap::Subcommand)]
pub enum ResearchCommand {
    /// Execute a Python snippet; output and cell written to research.ipynb
    Exec {
        code: String,
    },
    /// Execute a Python file; output and cell written to research.ipynb
    #[command(name = "exec-file")]
    ExecFile {
        path: PathBuf,
    },
    /// List variables in the kernel namespace
    Vars,
    /// Shutdown the project kernel
    Shutdown,
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

    let notebook = project_dir.join("research.ipynb");
    if !notebook.exists() {
        let project_name = project_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        let content = research_notebook(&project_name);
        std::fs::write(&notebook, content)
            .with_context(|| format!("Failed to create research.ipynb in '{}'", project_dir.display()))?;
        println!("Created research.ipynb in '{}'", project_dir.display());
    }

    let session = session_name(&project_dir);
    let python_dir = ensure_python_package()?;

    match args.command {
        None => kernel_start(&python_dir, &session, &project_dir, &notebook),
        Some(ResearchCommand::Exec { code }) =>
            kernel_exec(&python_dir, &session, &code, &notebook),
        Some(ResearchCommand::ExecFile { path }) => {
            let code = std::fs::read_to_string(&path)
                .with_context(|| format!("Cannot read {}", path.display()))?;
            kernel_exec(&python_dir, &session, &code, &notebook)
        }
        Some(ResearchCommand::Vars) =>
            kernel_vars(&python_dir, &session),
        Some(ResearchCommand::Shutdown) =>
            kernel_shutdown(&python_dir, &session),
    }
}

// ── Kernel operations ─────────────────────────────────────────────────────────

/// Start the kernel for this project.
/// On startup, execute all existing notebook cells to restore state.
fn kernel_start(
    python_dir: &Path,
    session: &str,
    project_dir: &Path,
    notebook: &Path,
) -> Result<()> {
    // Boot the kernel (no-op if already alive).
    run_lean_research(
        python_dir,
        project_dir,
        &["--session", session, "start"],
    )?;

    // Replay existing notebook cells to restore workspace state.
    let cells = read_notebook_cells(notebook)?;
    if !cells.is_empty() {
        println!("Replaying {} notebook cell(s) to restore state...", cells.len());
        for cell in &cells {
            run_lean_research(
                python_dir,
                project_dir,
                &["--session", session, "exec", cell],
            )?;
        }
        println!("Kernel ready. Session: {}", session);
    } else {
        println!("Kernel started. Session: {}  (empty notebook)", session);
    }

    Ok(())
}

/// Execute code in the kernel. Append result as a new cell in research.ipynb.
fn kernel_exec(
    python_dir: &Path,
    session: &str,
    code: &str,
    notebook: &Path,
) -> Result<()> {
    run_lean_research(
        python_dir,
        // exec needs to run from a neutral dir; notebook is abs path
        notebook.parent().unwrap_or(Path::new(".")),
        &["--session", session, "exec", code],
    )?;

    append_notebook_cell(notebook, code)?;
    Ok(())
}

fn kernel_vars(python_dir: &Path, session: &str) -> Result<()> {
    run_lean_research(python_dir, Path::new("."), &["--session", session, "vars"])
}

fn kernel_shutdown(python_dir: &Path, session: &str) -> Result<()> {
    run_lean_research(python_dir, Path::new("."), &["--session", session, "shutdown"])
}

// ── Notebook read/write ───────────────────────────────────────────────────────

/// Return the source of every code cell in the notebook (preserving order).
fn read_notebook_cells(notebook: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(notebook)
        .with_context(|| format!("Cannot read {}", notebook.display()))?;

    let nb: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("Invalid JSON in {}", notebook.display()))?;

    let mut cells = Vec::new();
    if let Some(arr) = nb["cells"].as_array() {
        for cell in arr {
            if cell["cell_type"].as_str() != Some("code") {
                continue;
            }
            let source = match cell["source"].as_array() {
                Some(lines) => lines
                    .iter()
                    .filter_map(|l| l.as_str())
                    .collect::<Vec<_>>()
                    .join(""),
                None => cell["source"].as_str().unwrap_or("").to_string(),
            };
            if !source.trim().is_empty() {
                cells.push(source);
            }
        }
    }
    Ok(cells)
}

/// Append a new code cell to the notebook file.
fn append_notebook_cell(notebook: &Path, code: &str) -> Result<()> {
    let text = std::fs::read_to_string(notebook)
        .with_context(|| format!("Cannot read {}", notebook.display()))?;

    let mut nb: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("Invalid JSON in {}", notebook.display()))?;

    let source_lines: Vec<serde_json::Value> = code
        .lines()
        .enumerate()
        .map(|(i, line)| {
            if i == 0 {
                serde_json::Value::String(line.to_string())
            } else {
                serde_json::Value::String(format!("\n{line}"))
            }
        })
        .collect();

    let new_cell = serde_json::json!({
        "cell_type": "code",
        "execution_count": null,
        "metadata": {},
        "outputs": [],
        "source": source_lines,
    });

    if let Some(cells) = nb["cells"].as_array_mut() {
        cells.push(new_cell);
    }

    let updated = serde_json::to_string_pretty(&nb)?;
    std::fs::write(notebook, updated)
        .with_context(|| format!("Cannot write {}", notebook.display()))?;

    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Session name = project directory basename.
fn session_name(project_dir: &Path) -> String {
    project_dir
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Run a `lean_research.cli` command via the embedded Python package.
fn run_lean_research(python_dir: &Path, cwd: &Path, lean_args: &[&str]) -> Result<()> {
    let argv: Vec<String> = std::iter::once("lean_research.cli".to_string())
        .chain(lean_args.iter().map(|s| s.to_string()))
        .collect();

    let set_argv = format!("import sys; sys.argv = {argv:?}");
    let code = format!("{set_argv}\nfrom lean_research.cli import main; main()");

    let status = python_cmd(python_dir)
        .arg("-c")
        .arg(&code)
        .current_dir(cwd)
        .status()
        .context("Failed to spawn python3")?;

    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Returns the `~/.lean/python/` directory after ensuring the embedded
/// lean_research package is extracted there.
fn ensure_python_package() -> Result<PathBuf> {
    let home = home_dir()?;
    let base = home.join(".lean").join("python");
    let pkg = base.join("lean_research");
    let kernel = pkg.join("kernel");

    std::fs::create_dir_all(&kernel)
        .context("Failed to create ~/.lean/python/lean_research/kernel")?;

    write_if_changed(&pkg.join("__init__.py"),    PY_INIT)?;
    write_if_changed(&pkg.join("session.py"),     PY_SESSION)?;
    write_if_changed(&pkg.join("cli.py"),         PY_CLI)?;
    write_if_changed(&kernel.join("__init__.py"), PY_KERNEL_INIT)?;
    write_if_changed(&kernel.join("startup.py"),  PY_KERNEL_STARTUP)?;

    ensure_py_deps()?;

    Ok(base)
}

/// Ensure `jupyter_client` and `ipykernel` are importable; pip-install if not.
fn ensure_py_deps() -> Result<()> {
    let already_installed = Command::new("python3")
        .args(["-c", "import jupyter_client, ipykernel"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if already_installed {
        return Ok(());
    }

    println!("Installing research dependencies (jupyter_client, ipykernel) ...");
    let status = Command::new("python3")
        .args(["-m", "pip", "install", "--quiet", "jupyter_client", "ipykernel"])
        .status()
        .context("Failed to run pip — is Python 3 installed?")?;

    if !status.success() {
        anyhow::bail!(
            "Failed to install jupyter_client and ipykernel.\n\
             Run manually: python3 -m pip install jupyter_client ipykernel"
        );
    }

    Ok(())
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("USERPROFILE").map(PathBuf::from))
        .context("HOME env not set")
}

fn write_if_changed(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        if let Ok(existing) = std::fs::read_to_string(path) {
            if existing == content {
                return Ok(());
            }
        }
    }
    std::fs::write(path, content)
        .with_context(|| format!("Failed to write {}", path.display()))
}

fn python_cmd(python_dir: &Path) -> Command {
    let mut cmd = Command::new("python3");
    let existing = std::env::var("PYTHONPATH").unwrap_or_default();
    let path = if existing.is_empty() {
        python_dir.display().to_string()
    } else {
        format!("{}:{}", python_dir.display(), existing)
    };
    cmd.env("PYTHONPATH", path);
    cmd
}
