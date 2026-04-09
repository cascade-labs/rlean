"""
lean-research CLI — manages and interacts with a persistent Lean research kernel.

Usage:
  lean-research start [session-name]     Start a new kernel session
  lean-research attach [session-name]    Attach to existing session
  lean-research exec "code"              Execute code in current session
  lean-research exec-file path.py        Execute a Python file
  lean-research vars                     List variables in kernel namespace
  lean-research sessions                 List all sessions
  lean-research interrupt                Interrupt running execution
  lean-research shutdown [session-name]  Shutdown a kernel
"""

import argparse
import json
import sys
import os
import subprocess
import time
from pathlib import Path
from typing import Optional

def get_client(session_name: str):
    """Get a BlockingKernelClient connected to the named session."""
    from jupyter_client import BlockingKernelClient
    from .session import conn_file, is_alive

    cf = conn_file(session_name)
    if not cf.exists():
        print(f"No session '{session_name}' found. Run: lean-research start {session_name}", file=sys.stderr)
        sys.exit(1)
    if not is_alive(session_name):
        print(f"Session '{session_name}' kernel has died. Run: lean-research start {session_name}", file=sys.stderr)
        sys.exit(1)

    client = BlockingKernelClient(connection_file=str(cf))
    client.load_connection_file()
    client.start_channels()
    return client

def cmd_start(args):
    """Start a new kernel session."""
    from jupyter_client import KernelManager
    from .session import conn_file, save_pid, is_alive, session_dir
    import lean_research.kernel as kernel_pkg

    name = args.session

    if is_alive(name):
        print(f"Session '{name}' is already running.")
        return

    # Find the startup script
    startup_script = Path(kernel_pkg.__file__).parent / "startup.py"

    km = KernelManager()
    # Set connection file location
    km.connection_file = str(conn_file(name))

    # Launch with PYTHONSTARTUP pointing to our startup script
    env = os.environ.copy()
    env["PYTHONSTARTUP"] = str(startup_script)

    km.start_kernel(extra_arguments=[], env=env)

    # Save PID
    save_pid(name, km.kernel.pid)

    print(f"Started session '{name}' (PID {km.kernel.pid})")
    print(f"Connection file: {conn_file(name)}")

def cmd_exec(args, code: Optional[str] = None):
    """Execute code in the kernel and print output."""
    client = get_client(args.session)

    if code is None:
        code = args.code

    try:
        msg_id = client.execute(code)

        # Collect output
        while True:
            try:
                msg = client.get_iopub_msg(timeout=30)
            except Exception:
                print("Timeout waiting for kernel response.", file=sys.stderr)
                break

            msg_type = msg["msg_type"]
            content = msg.get("content", {})

            if msg_type == "stream":
                print(content.get("text", ""), end="")

            elif msg_type == "execute_result":
                data = content.get("data", {})
                text = data.get("text/plain", "")
                if text:
                    print(text)

            elif msg_type == "display_data":
                data = content.get("data", {})
                text = data.get("text/plain", "")
                if text:
                    print(text)

            elif msg_type == "error":
                ename = content.get("ename", "Error")
                evalue = content.get("evalue", "")
                traceback = content.get("traceback", [])
                # Strip ANSI codes for clean CLI output
                import re
                ansi_escape = re.compile(r'\x1B(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])')
                for line in traceback:
                    print(ansi_escape.sub("", line), file=sys.stderr)
                break

            elif msg_type == "status":
                if content.get("execution_state") == "idle":
                    break

        # Get the reply to check for errors
        reply = client.get_shell_msg(timeout=10)
        status = reply.get("content", {}).get("status", "ok")
        if status == "error":
            sys.exit(1)

    finally:
        client.stop_channels()

def cmd_exec_file(args):
    """Execute a Python file in the kernel."""
    path = Path(args.path)
    if not path.exists():
        print(f"File not found: {path}", file=sys.stderr)
        sys.exit(1)
    code = path.read_text()
    cmd_exec(args, code=code)

def cmd_vars(args):
    """List variables in the kernel namespace."""
    code = """
import pandas as pd
_vars = {}
for _name, _val in list(globals().items()):
    if _name.startswith('_'):
        continue
    _t = type(_val).__name__
    if isinstance(_val, pd.DataFrame):
        _vars[_name] = f"DataFrame  shape={_val.shape}"
    elif isinstance(_val, (list, dict, tuple)):
        _vars[_name] = f"{_t}  len={len(_val)}"
    else:
        _vars[_name] = _t
for _k, _v in sorted(_vars.items()):
    print(f"  {_k:<20} {_v}")
del _vars, _name, _val, _t, _k, _v
"""
    cmd_exec(args, code=code)

def cmd_sessions(args):
    """List all sessions."""
    from .session import list_sessions
    sessions = list_sessions()
    if not sessions:
        print("No sessions found.")
        return
    print(f"{'Name':<20} {'Status':<10} {'PID':<8}")
    print("-" * 40)
    for s in sessions:
        status = "alive" if s["alive"] else "dead"
        pid = str(s["pid"]) if s["pid"] else "?"
        print(f"{s['name']:<20} {status:<10} {pid:<8}")

def cmd_interrupt(args):
    """Interrupt the running execution."""
    from jupyter_client import BlockingKernelClient
    from .session import conn_file
    client = get_client(args.session)
    try:
        client.interrupt_kernel()
        print("Interrupted.")
    finally:
        client.stop_channels()

def cmd_shutdown(args):
    """Shutdown a kernel gracefully."""
    from .session import conn_file, cleanup_session, is_alive
    name = args.session
    if not is_alive(name):
        print(f"Session '{name}' is not running.")
        cleanup_session(name)
        return
    client = get_client(name)
    try:
        client.shutdown()
        time.sleep(1)
        print(f"Session '{name}' shut down.")
    finally:
        client.stop_channels()
    cleanup_session(name)

DEFAULT_SESSION = "default"

def main():
    parser = argparse.ArgumentParser(
        prog="lean-research",
        description="Lean Research kernel manager"
    )
    parser.add_argument("--session", "-s", default=DEFAULT_SESSION, help="Session name")

    sub = parser.add_subparsers(dest="command")

    p_start = sub.add_parser("start", help="Start a kernel session")

    p_attach = sub.add_parser("attach", help="Attach to existing session (verifies it's alive)")

    p_exec = sub.add_parser("exec", help="Execute code")
    p_exec.add_argument("code", help="Python code to execute")

    p_file = sub.add_parser("exec-file", help="Execute a Python file")
    p_file.add_argument("path", help="Path to .py file")

    p_vars = sub.add_parser("vars", help="List variables in namespace")

    p_sessions = sub.add_parser("sessions", help="List all sessions")

    p_interrupt = sub.add_parser("interrupt", help="Interrupt running execution")

    p_shutdown = sub.add_parser("shutdown", help="Shutdown kernel")

    args = parser.parse_args()

    if args.command == "start":
        cmd_start(args)
    elif args.command == "attach":
        from .session import is_alive
        if is_alive(args.session):
            print(f"Session '{args.session}' is alive.")
        else:
            print(f"Session '{args.session}' is not running.")
            sys.exit(1)
    elif args.command == "exec":
        cmd_exec(args)
    elif args.command == "exec-file":
        cmd_exec_file(args)
    elif args.command == "vars":
        cmd_vars(args)
    elif args.command == "sessions":
        cmd_sessions(args)
    elif args.command == "interrupt":
        cmd_interrupt(args)
    elif args.command == "shutdown":
        cmd_shutdown(args)
    else:
        parser.print_help()

if __name__ == "__main__":
    main()
