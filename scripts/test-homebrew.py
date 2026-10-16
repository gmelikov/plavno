#!/usr/bin/env python3
"""Install the generated formula against local release archives, then brew test."""

import argparse
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import subprocess
from threading import Thread


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--formula", required=True, type=Path)
    parser.add_argument("--archives", required=True, type=Path)
    parser.add_argument("--tag", required=True)
    args = parser.parse_args()
    tap = "gmelikov/plavno-ci"
    subprocess.run(["brew", "tap-new", tap], check=True)
    repository = Path(subprocess.check_output(["brew", "--repository", tap], text=True).strip())
    handler = partial(SimpleHTTPRequestHandler, directory=str(args.archives.resolve()))
    with ThreadingHTTPServer(("127.0.0.1", 0), handler) as server:
        thread = Thread(target=server.serve_forever, daemon=True)
        thread.start()
        original = f"https://github.com/gmelikov/plavno/releases/download/{args.tag}"
        formula = args.formula.read_text()
        if formula.count(original + "/") != 2:
            raise RuntimeError("Expected both architecture URLs in the generated formula")
        # The formula and checksums stay identical; only the download host changes.
        formula = formula.replace(original, f"http://127.0.0.1:{server.server_port}")
        (repository / "Formula" / "plavno.rb").write_text(formula)
        try:
            subprocess.run(["brew", "install", "--formula", f"{tap}/plavno"], check=True)
            subprocess.run(["brew", "test", "--verbose", f"{tap}/plavno"], check=True)
        finally:
            server.shutdown()
            thread.join()


if __name__ == "__main__":
    main()
