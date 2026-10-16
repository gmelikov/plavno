#!/usr/bin/env python3
"""Generate the macOS binary formula from a stable release's SHA256SUMS."""

import argparse
from pathlib import Path
import re


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag", required=True)
    parser.add_argument("--checksums", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)", args.tag):
        parser.error("Homebrew requires a stable vMAJOR.MINOR.PATCH tag")
    version = args.tag[1:]
    checksums = {}
    for line in args.checksums.read_text().splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9_.+-]+)", line)
        if not match or match[2] in checksums:
            parser.error("invalid or duplicate SHA256SUMS entry")
        checksums[match[2]] = match[1]
    names = {arch: f"plavno-{version}-macos-{arch}.tar.gz" for arch in ("aarch64", "x86_64")}
    if any(name not in checksums for name in names.values()):
        parser.error("SHA256SUMS must contain both macOS archives")
    base = f"https://github.com/gmelikov/plavno/releases/download/{args.tag}"
    formula = f'''class Plavno < Formula
  desc "Terminal music player with service waves and library shuffle"
  homepage "https://github.com/gmelikov/plavno"
  version "{version}"
  license "Apache-2.0"

  depends_on :macos

  on_macos do
    on_arm do
      url "{base}/{names['aarch64']}"
      sha256 "{checksums[names['aarch64']]}"
    end

    on_intel do
      url "{base}/{names['x86_64']}"
      sha256 "{checksums[names['x86_64']]}"
    end
  end

  def install
    bin.install "plavno", "plavno-spotify", "plavno-youtube"
  end

  def caveats
    <<~EOS
      Install ffmpeg with `brew install ffmpeg` for HLS playback.
      Browser-based sign-in needs Chrome or Chromium.
      The prebuilt binaries are tested on macOS 15.
    EOS
  end

  test do
    assert_match "plavno #{{version}}", shell_output("#{{bin}}/plavno --version")
    assert_match "plavno --source SOURCE", shell_output("#{{bin}}/plavno --help")
    %w[plavno-spotify plavno-youtube].each do |helper|
      assert_match '"error":', pipe_output("#{{bin}}/#{{helper}}", "{{}}\\n", 0)
    end
  end
end
'''
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(formula, encoding="utf-8")


if __name__ == "__main__":
    main()
