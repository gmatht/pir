"""Build the WASM app, serve it locally, and open a browser.

Usage:
    python scripts/wasm.py [example] [port]

The default example is "app".
"""

import http.server
import mimetypes
import os
import shutil
import subprocess
import sys
import threading
import webbrowser

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
PKG = os.path.join(ROOT, "rustxwidgets")

EXAMPLE = sys.argv[1] if len(sys.argv) > 1 else "app"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8000

BUILD_DIR = os.path.join(ROOT, "target", "wasm32-unknown-unknown", "release", "examples")
OUT_DIR = os.path.join(ROOT, "target", "wasm-out")


def run(cmd, **kwargs):
    print(f"+ {' '.join(cmd)}")
    subprocess.check_call(cmd, **kwargs)


def main():
    # 1. Ensure wasm-bindgen-cli is available.
    def find_wb_version():
        try:
            out = subprocess.check_output(["wasm-bindgen", "--version"], stderr=subprocess.STDOUT)
            return out.decode().strip().split()[-1]
        except (FileNotFoundError, subprocess.CalledProcessError):
            return None

    wb_ver = find_wb_version()
    if wb_ver is None:
        print("wasm-bindgen not found – installing...")
        run(["cargo", "install", "wasm-bindgen-cli"])
        wb_ver = find_wb_version()
        if wb_ver is None:
            print("Error: failed to install wasm-bindgen-cli", file=sys.stderr)
            sys.exit(1)

    # 2. Build the example for WASM.
    run(["cargo", "build", "--target", "wasm32-unknown-unknown",
         "--release", "--example", EXAMPLE],
        cwd=PKG)

    wasm_file = os.path.join(BUILD_DIR, f"{EXAMPLE}.wasm")
    if not os.path.exists(wasm_file):
        print(f"Error: {wasm_file} not found", file=sys.stderr)
        sys.exit(1)

    # 3. Generate JS bindings with wasm-bindgen.
    if os.path.exists(OUT_DIR):
        shutil.rmtree(OUT_DIR)
    os.makedirs(OUT_DIR)
    run(["wasm-bindgen", wasm_file,
         "--out-dir", OUT_DIR,
         "--target", "web",
         "--out-name", f"wasm_{EXAMPLE}"])

    # 4. Copy the HTML file into the output directory.
    html_src = os.path.join(PKG, "examples", f"wasm_{EXAMPLE}.html")
    if os.path.exists(html_src):
        shutil.copy2(html_src, os.path.join(OUT_DIR, f"wasm_{EXAMPLE}.html"))

    # 5. Start the HTTP server on a background thread.
    os.chdir(OUT_DIR)
    mimetypes.add_type("application/wasm", ".wasm")
    mimetypes.add_type("text/javascript", ".js")

    class WasmHandler(http.server.SimpleHTTPRequestHandler):
        pass

    def serve():
        httpd = http.server.HTTPServer(("0.0.0.0", PORT), WasmHandler)
        print(f"Serving at http://localhost:{PORT}/")
        httpd.serve_forever()

    t = threading.Thread(target=serve, daemon=True)
    t.start()

    # 6. Open the browser.
    url = f"http://localhost:{PORT}/wasm_{EXAMPLE}.html"
    print(f"Opening {url} ...")
    webbrowser.open(url)

    print("Press Ctrl+C to stop.")
    try:
        t.join()
    except KeyboardInterrupt:
        print("\nStopped.")


if __name__ == "__main__":
    main()
