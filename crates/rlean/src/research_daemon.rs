//! `rlean __research-daemon` — persistent PyO3 research process.
//!
//! Started by `rlean research <project>` as a detached background process.
//! Maintains a persistent Python interpreter with QuantBook available as `qb`.
//! All matplotlib figures are captured after each exec and returned as base64 PNG.
//!
//! Protocol: newline-delimited JSON over a Unix domain socket.
//!
//! Request:
//!   {"op": "exec",     "code": "<python code>"}
//!   {"op": "vars"}
//!   {"op": "ping"}
//!   {"op": "shutdown"}
//!
//! Response:
//!   {"status": "ok",    "stdout": "...", "stderr": "...", "figures": ["base64..."]}
//!   {"status": "pong"}
//!   {"status": "error", "message": "..."}

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use serde::{Deserialize, Serialize};

// ── CLI args ──────────────────────────────────────────────────────────────────

#[derive(clap::Args)]
pub struct ResearchDaemonArgs {
    /// Session name — used for socket/PID paths under ~/.lean-research/sessions/
    #[arg(long)]
    pub session: String,

    /// Absolute path to the project directory
    #[arg(long)]
    pub project: PathBuf,

    /// Root data folder passed to QuantBook.set_data_folder
    #[arg(long)]
    pub data_folder: Option<PathBuf>,
}

// ── Wire protocol ─────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Req {
    op: String,
    #[serde(default)]
    code: String,
}

#[derive(Serialize, Default)]
pub struct Resp {
    pub status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stdout: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub stderr: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub figures: Vec<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
}

impl Resp {
    pub fn ok(stdout: String, stderr: String, figures: Vec<String>) -> Self {
        Resp {
            status: "ok".into(),
            stdout,
            stderr,
            figures,
            ..Default::default()
        }
    }
    pub fn pong() -> Self {
        Resp {
            status: "pong".into(),
            ..Default::default()
        }
    }
    pub fn error(msg: impl Into<String>) -> Self {
        Resp {
            status: "error".into(),
            message: msg.into(),
            ..Default::default()
        }
    }
}

// ── Session directory helpers (also used by research.rs) ─────────────────────

pub fn session_dir(name: &str) -> Result<PathBuf> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .context("HOME not set")?;
    let dir = home.join(".lean-research").join("sessions").join(name);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("Failed to create session dir: {}", dir.display()))?;
    Ok(dir)
}

pub fn sock_path(session: &str) -> Result<PathBuf> {
    Ok(session_dir(session)?.join("server.sock"))
}

// ── Python startup code ───────────────────────────────────────────────────────

fn startup_code(data_folder: Option<&Path>) -> String {
    let set_data = match data_folder {
        Some(p) => format!(r#"qb.set_data_folder(r"{}")"#, p.display()),
        None => String::new(),
    };

    // NOTE: All private names use _-prefix so they don't pollute `vars` output.
    format!(
        r#"
import sys as _sys
import io as _io

# Non-interactive backend — must be set before any pyplot import.
import matplotlib as _mpl
_mpl.use('Agg')
import matplotlib.pyplot as plt
plt.show = lambda *a, **kw: None   # figures harvested by Rust after each exec

# Research classes
try:
    from AlgorithmImports import QuantBook, Resolution, QCAlgorithm
    import numpy as np
    import pandas as pd
    qb = QuantBook()
    {set_data}
    _sys.__stdout__.write("Research kernel ready.  Available: qb, QuantBook, Resolution, np, pd, plt\n")
    _sys.__stdout__.flush()
except ImportError as _e:
    _sys.__stderr__.write(f"Warning: could not import AlgorithmImports: {{_e}}\n")
    _sys.__stderr__.flush()
    qb = None
    Resolution = None

# ── Exec capture helper ──────────────────────────────────────────────────────
def _rlean_exec_capture(_code_str, _glb):
    """Execute `_code_str` in `_glb`, capture stdout/stderr, harvest figures."""
    import sys, io, traceback, base64
    import matplotlib.pyplot as _plt

    _buf_out = io.StringIO()
    _buf_err = io.StringIO()
    sys.stdout = _buf_out
    sys.stderr = _buf_err
    try:
        exec(compile(_code_str, '<research>', 'exec'), _glb)
    except Exception:
        traceback.print_exc()
    finally:
        sys.stdout = sys.__stdout__
        sys.stderr = sys.__stderr__

    # Harvest all open matplotlib figures into base64 PNG strings.
    _figs = []
    for _num in _plt.get_fignums():
        _fig = _plt.figure(_num)
        _buf = io.BytesIO()
        _fig.savefig(_buf, format='png', dpi=150, bbox_inches='tight')
        _buf.seek(0)
        _figs.append(base64.b64encode(_buf.read()).decode('utf-8'))
    _plt.close('all')

    return _buf_out.getvalue(), _buf_err.getvalue(), _figs
"#
    )
}

// ── Vars inspection code ──────────────────────────────────────────────────────

const VARS_CODE: &str = r#"
_vd = {}
for _k, _v in list(globals().items()):
    if _k.startswith('_'):
        continue
    _t = type(_v).__name__
    try:
        import pandas as _pd
        if isinstance(_v, _pd.DataFrame):
            _vd[_k] = f"DataFrame  shape={_v.shape}"
            continue
    except ImportError:
        pass
    if isinstance(_v, (list, dict, tuple)):
        _vd[_k] = f"{_t}  len={len(_v)}"
    else:
        _vd[_k] = _t
for _k, _v in sorted(_vd.items()):
    print(f"  {_k:<20} {_v}")
del _vd, _k, _v, _t
"#;

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn run_daemon(args: ResearchDaemonArgs) -> Result<()> {
    use lean_python::AlgorithmImports;

    let sdir = session_dir(&args.session)?;
    let sock = sdir.join("server.sock");
    let pid_path = sdir.join("pid");

    // Remove stale socket from a previous run.
    let _ = std::fs::remove_file(&sock);

    // Write PID so the client can check liveness.
    std::fs::write(&pid_path, std::process::id().to_string()).context("Failed to write PID")?;

    // Register AlgorithmImports before Python initialises.
    pyo3::append_to_inittab!(AlgorithmImports);
    pyo3::Python::initialize();

    // Create persistent globals dict and run startup.
    let globals: Py<PyDict> = Python::attach(|py| {
        let d = PyDict::new(py);
        let code = startup_code(args.data_folder.as_deref());
        let builtins = PyModule::import(py, "builtins")?;
        let exec_fn = builtins.getattr("exec")?;
        exec_fn.call1((&code as &str, &d))?;
        Ok::<_, PyErr>(d.unbind())
    })?;

    // Bind the socket — the client's wait_for_daemon poll unblocks after this.
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("Failed to bind socket: {}", sock.display()))?;

    eprintln!(
        "[research-daemon] ready  session='{}' sock='{}'",
        args.session,
        sock.display()
    );

    // ── Accept loop ───────────────────────────────────────────────────────────
    for incoming in listener.incoming() {
        let mut stream = match incoming {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };

        let mut line = String::new();
        let mut reader = BufReader::new(stream.try_clone()?);
        if reader.read_line(&mut line).is_err() {
            continue;
        }

        let req: Req = match serde_json::from_str(line.trim()) {
            Ok(r) => r,
            Err(e) => {
                let _ = write_resp(&mut stream, &Resp::error(format!("bad JSON: {e}")));
                continue;
            }
        };

        let resp = match req.op.as_str() {
            "ping" => Resp::pong(),

            "shutdown" => {
                let r = Resp {
                    status: "ok".into(),
                    stdout: "Shutting down.".into(),
                    ..Default::default()
                };
                let _ = write_resp(&mut stream, &r);
                let _ = std::fs::remove_file(&sock);
                let _ = std::fs::remove_file(&pid_path);
                std::process::exit(0);
            }

            "exec" | "vars" => {
                let code = if req.op == "vars" {
                    VARS_CODE.to_string()
                } else {
                    req.code
                };
                exec_in_globals(&globals, &code)
            }

            other => Resp::error(format!("unknown op: {other}")),
        };

        let _ = write_resp(&mut stream, &resp);
    }

    Ok(())
}

// ── Execute code in the persistent globals dict ───────────────────────────────

fn exec_in_globals(globals: &Py<PyDict>, code: &str) -> Resp {
    Python::attach(|py| {
        let g = globals.bind(py);

        let capture_fn = match g.get_item("_rlean_exec_capture") {
            Ok(Some(f)) => f,
            _ => return Resp::error("_rlean_exec_capture not found — startup may have failed"),
        };

        match capture_fn.call1((code, g)) {
            Ok(result) => match result.extract::<(String, String, Vec<String>)>() {
                Ok((out, err, figs)) => Resp::ok(out, err, figs),
                Err(e) => Resp::error(format!("extract error: {e}")),
            },
            Err(e) => {
                // Python exception escaped capture — shouldn't happen, but handle gracefully.
                let tb = e
                    .traceback(py)
                    .and_then(|t| t.format().ok())
                    .unwrap_or_default();
                Resp {
                    status: "ok".into(),
                    stderr: format!("{e}\n{tb}"),
                    ..Default::default()
                }
            }
        }
    })
}

// ── Write one JSON response line ──────────────────────────────────────────────

fn write_resp(stream: &mut std::os::unix::net::UnixStream, resp: &Resp) -> Result<()> {
    let mut line = serde_json::to_string(resp)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    Ok(())
}
