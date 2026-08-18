"""
Standalone YouTube Music login helper for Windows.
Uses Edge WebView2 via pywebview to capture auth cookies,
then writes them to a JSON file for Mixtapes to import.

Usage:
  login_helper.exe [--output PATH]

Writes captured headers JSON to:
  --output PATH   (default: %LOCALAPPDATA%/Mixtapes/login_headers.json)
"""

import json
import os
import sys
import time
import webview


OUTPUT_PATH = None


def get_default_output():
    # Match the app's auth path: GLib.get_user_data_dir() + "/muse/headers_auth.json"
    # On Windows: %LOCALAPPDATA%/muse/headers_auth.json
    # On Linux: ~/.local/share/muse/headers_auth.json
    appdata = os.environ.get("LOCALAPPDATA", "")
    if not appdata:
        appdata = os.path.join(os.path.expanduser("~"), ".local", "share")
    d = os.path.join(appdata, "muse")
    os.makedirs(d, exist_ok=True)
    return os.path.join(d, "headers_auth.json")


def check_cookies(window):
    """Runs in pywebview's background thread. Polls for auth cookies."""
    for attempt in range(120):
        time.sleep(1)
        try:
            url = window.get_current_url() or ""
        except Exception:
            continue

        if "music.youtube.com" not in url or "accounts.google.com" in url:
            continue

        print(f"[attempt {attempt}] On YouTube Music: {url}")

        # Strategy 1: pywebview get_cookies() — gets all cookies including HttpOnly
        try:
            cookies = window.get_cookies()
            print(f"  get_cookies() returned {len(cookies)} cookies")

            cookie_strs = []
            has_sapisid = False

            for cookie in cookies:
                # pywebview 4.x returns SimpleCookie dicts, 5.x returns Cookie objects
                if hasattr(cookie, "items"):
                    items = cookie.items()
                elif hasattr(cookie, "name") and hasattr(cookie, "value"):
                    items = [(cookie.name, cookie)]
                else:
                    continue

                for name, morsel in items:
                    value = getattr(morsel, "value", morsel)
                    if not isinstance(value, str):
                        value = str(value)
                    if name and value:
                        cookie_strs.append(f"{name}={value}")
                        if name in ("SAPISID", "__Secure-3PAPISID"):
                            has_sapisid = True
                            print(f"  Found auth cookie: {name}={value}")

            if has_sapisid:
                _save_and_close(window, "; ".join(cookie_strs))
                return

        except Exception as e:
            print(f"  get_cookies() error: {e}")

        # Strategy 2: document.cookie — gets non-HttpOnly cookies (SAPISID is accessible)
        try:
            js_cookies = window.evaluate_js("document.cookie")
            if js_cookies:
                print(f"  document.cookie length: {len(js_cookies)}")
                if "SAPISID" in js_cookies:
                    print("  Found SAPISID via document.cookie!")
                    _save_and_close(window, js_cookies)
                    return
        except Exception as e:
            print(f"  evaluate_js error: {e}")

    print("Timed out waiting for auth cookies (120s).")


def _compute_sapisidhash(sapisid, origin="https://music.youtube.com"):
    """Compute SAPISIDHASH from SAPISID cookie, matching what YouTube expects."""
    import hashlib
    timestamp = str(int(time.time()))
    hash_input = f"{timestamp} {sapisid} {origin}"
    sha1 = hashlib.sha1(hash_input.encode()).hexdigest()
    return f"SAPISIDHASH {timestamp}_{sha1}"


def _extract_sapisid(cookie_string):
    """Extract SAPISID value from a cookie string."""
    for part in cookie_string.split(";"):
        part = part.strip()
        if part.startswith("SAPISID="):
            val = part[len("SAPISID="):]
            return val.strip().strip('"').strip("'")
    return None


def _save_and_close(window, cookie_string):
    ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
    try:
        result = window.evaluate_js("navigator.userAgent")
        if result:
            ua = result
    except Exception:
        pass

    # Build full headers matching what ytmusicapi expects for browser auth
    sapisid = _extract_sapisid(cookie_string)
    auth = _compute_sapisidhash(sapisid) if sapisid else ""

    headers = {
        "User-Agent": ua,
        "Accept": "*/*",
        "Accept-Language": "en-US,en;q=0.9",
        "Content-Type": "application/json; charset=UTF-8",
        "Authorization": auth,
        "X-Goog-AuthUser": "0",
        "X-Origin": "https://music.youtube.com",
        "Origin": "https://music.youtube.com",
        "Referer": "https://music.youtube.com/",
        "Cookie": cookie_string,
    }

    output = OUTPUT_PATH or get_default_output()
    try:
        os.makedirs(os.path.dirname(output), exist_ok=True)
        with open(output, "w") as f:
            json.dump(headers, f)
        print(f"Login successful! Headers saved to: {output}")
    except Exception as e:
        print(f"Failed to save headers to {output}: {e}")
    finally:
        window.destroy()


def main():
    global OUTPUT_PATH

    args = sys.argv[1:]
    for i, arg in enumerate(args):
        if arg == "--output" and i + 1 < len(args):
            OUTPUT_PATH = args[i + 1]

    window = webview.create_window(
        "Mixtapes - Login to YouTube Music",
        "https://accounts.google.com/ServiceLogin?ltmpl=music&service=youtube"
        "&uilel=3&passive=true"
        "&continue=https%3A%2F%2Fmusic.youtube.com%2Flibrary",
        width=700,
        height=600,
    )

    webview.start(func=check_cookies, args=(window,), private_mode=True)

    output = OUTPUT_PATH or get_default_output()
    if not os.path.exists(output):
        print("Login was cancelled or failed.")
        sys.exit(1)


if __name__ == "__main__":
    main()
