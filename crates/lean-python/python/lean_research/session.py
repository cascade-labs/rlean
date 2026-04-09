import json
import os
import signal
from pathlib import Path
from typing import Optional

SESSIONS_DIR = Path.home() / ".lean-research" / "sessions"

def sessions_dir() -> Path:
    SESSIONS_DIR.mkdir(parents=True, exist_ok=True)
    return SESSIONS_DIR

def session_dir(name: str) -> Path:
    d = sessions_dir() / name
    d.mkdir(parents=True, exist_ok=True)
    return d

def conn_file(name: str) -> Path:
    return session_dir(name) / "kernel.json"

def pid_file(name: str) -> Path:
    return session_dir(name) / "pid"

def save_pid(name: str, pid: int):
    pid_file(name).write_text(str(pid))

def load_pid(name: str) -> Optional[int]:
    p = pid_file(name)
    if p.exists():
        try:
            return int(p.read_text().strip())
        except ValueError:
            return None
    return None

def is_alive(name: str) -> bool:
    pid = load_pid(name)
    if pid is None:
        return False
    try:
        os.kill(pid, 0)  # signal 0 = check existence
        return True
    except (ProcessLookupError, PermissionError):
        return False

def list_sessions() -> list[dict]:
    if not SESSIONS_DIR.exists():
        return []
    results = []
    for d in SESSIONS_DIR.iterdir():
        if d.is_dir():
            name = d.name
            alive = is_alive(name)
            pid = load_pid(name)
            has_conn = conn_file(name).exists()
            results.append({"name": name, "alive": alive, "pid": pid, "has_conn": has_conn})
    return results

def cleanup_session(name: str):
    import shutil
    d = session_dir(name)
    if d.exists():
        shutil.rmtree(d)
