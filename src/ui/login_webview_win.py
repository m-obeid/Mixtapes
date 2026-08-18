"""
Windows login helper integration.
Launches a standalone login helper and watches
for the resulting credentials file.
"""

import os
import subprocess
import threading
import sys


def get_login_output_path():
    appdata = os.environ.get("LOCALAPPDATA", os.path.expanduser("~"))
    return os.path.join(appdata, "Mixtapes", "login_headers.json")


def find_login_helper():
    """Find MixtapesLogin.exe relative to the app."""
    base = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    if sys.platform == "win32":
        candidates = [
            os.path.join(base, "windows", "MixtapesLogin.exe"),
            os.path.join(base, "MixtapesLogin.exe"),
        ]
    else:
        candidates = [
            os.path.join(base, "windows", "login_helper.py"),
            os.path.join(base, "MixtapesLogin.exe"),
        ]
    for c in candidates:
        if os.path.exists(c):
            return c
    return None


def launch_login(on_complete):
    """
    Launch the login helper and call on_complete(headers_json_str) when done.
    on_complete is called from a background thread — use GLib.idle_add to
    marshal to the GTK main thread.
    """
    helper = find_login_helper()
    if not helper:
        on_complete(None, "Login helper not found")
        return

    output_path = get_login_output_path()
    os.makedirs(os.path.dirname(output_path), exist_ok=True)
    # Remove stale file
    if os.path.exists(output_path):
        os.remove(output_path)

    def _run():
        try:
            if helper.endswith(".py"):
                cmd = [sys.executable, helper, "--output", output_path]
            else:
                cmd = [helper, "--output", output_path]
            
            subprocess.run(
                cmd,
                timeout=300,
            )
            if os.path.exists(output_path):
                with open(output_path, "r") as f:
                    headers = f.read()
                on_complete(headers, None)
            else:
                on_complete(None, "Login was cancelled or failed")
        except subprocess.TimeoutExpired:
            on_complete(None, "Login timed out")
        except Exception as e:
            on_complete(None, str(e))

    thread = threading.Thread(target=_run, daemon=True)
    thread.start()
