#!/usr/bin/env python3
"""The third-party notices: what the apps show in Settings → Licenses,
and what the site publishes as web/licenses.html.

Two sources, merged into one list:

- every Rust crate any shipped build links (desktop, phones, server),
  with its license text, from cargo-about (`about.toml` at the root
  says which targets and which license to take where a crate offers
  several); our own workspace crates are left out;
- the native code cargo cannot see, listed in COMPONENTS below with its
  license file under licenses/native/ (fetched from each project's own
  repository): Skia and what it bundles, FFmpeg and x264 in the desktop
  builds, libopus, the GPU encoders' headers, the Lucide icons — and
  Slint, whose crates carry no license file.

CI runs it before every build (.github/workflows/licenses.yml), so the
releases and the site always carry the list for the commit they are
built from. By hand (`cargo install cargo-about --features cli` once):

    python3 scripts/licenses.py

writes crates/frontend/licenses.json (embedded in the apps) and
web/licenses.html — worth committing now and then, since a local build
shows the committed copy.
"""

import argparse
import html
import json
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
NATIVE = ROOT / "licenses" / "native"
APP_JSON = ROOT / "crates" / "frontend" / "licenses.json"
SITE_HTML = ROOT / "web" / "licenses.html"
SOURCE = "https://github.com/mpwsh/baucord"

# Our crates: not third-party. (third_party/wasapi is a patched copy of
# someone else's crate and stays in.)
OWN = {
    "android", "app-core", "audio", "client", "desktop", "frontend", "i18n",
    "ios", "protocol", "server", "transport", "ui", "video",
}

# The native code, and baucord itself first. `texts` are files under
# licenses/native (or the repo's LICENSE), in order; `note` is the
# copyright line and where the source is.
COMPONENTS = [
    {
        "name": "baucord",
        "license": "Apache License 2.0",
        "note": f"Copyright 0x0 Studios. Source: {SOURCE}",
        "texts": ["../../LICENSE"],
    },
    {
        "name": "The desktop builds (Windows, macOS, Linux)",
        "license": "GNU General Public License 3.0",
        "note": (
            "The desktop builds include FFmpeg and x264 (below), which are "
            "under the GNU GPL version 2 or later, so a desktop build as a "
            "whole is distributed under the GNU General Public License "
            "version 3. baucord's own source stays under the Apache License "
            "2.0. The phone apps contain neither. The complete source of "
            "every release, including the scripts that build FFmpeg and x264 "
            f"into it (.github/workflows/release.yml), is at {SOURCE} — each "
            "release is a tag there."
        ),
        "texts": ["GPL-3.0.txt"],
    },
    {
        "name": "Slint",
        "license": "Slint Royalty-free Desktop, Mobile, and Web Applications License 2.0",
        "note": (
            "Copyright © SixtyFPS GmbH. https://slint.dev — used under the "
            "royalty-free license; its attribution is in Settings → About. "
            "Crates: CRATES"
        ),
        "texts": ["slint-royalty-free-2.0.md"],
        "crates_like": r"^(slint|slint-macros|i-slint-.*)$",
    },
    {
        "name": "FFmpeg (desktop builds)",
        "license": "GNU General Public License 2.0 or later",
        "note": (
            "Copyright © the FFmpeg developers. https://ffmpeg.org — built "
            "with --enable-gpl and libx264, which puts it under the GPL. "
            "Source: FFmpeg 7.1.1, https://ffmpeg.org/releases/ffmpeg-7.1.1.tar.xz "
            "(Linux, macOS); FFmpeg 7.1.2 from vcpkg at commit "
            "34823ada10080ddca99b60e85f80f55e18a44eea (Windows)."
        ),
        "texts": ["ffmpeg.md", "GPL-2.0.txt"],
    },
    {
        "name": "x264 (desktop builds)",
        "license": "GNU General Public License 2.0 or later",
        "note": (
            "Copyright © 2003-2024 x264 project. "
            "https://www.videolan.org/developers/x264.html — source: commit "
            "31e19f92f00c7003fa115047ce50978bc98c3a0d (0.164.3108) of "
            "https://code.videolan.org/videolan/x264. The license text is "
            "the GPL 2.0 under FFmpeg."
        ),
        "texts": [],
    },
    {
        "name": "NVIDIA Video Codec SDK headers (Windows)",
        "license": "MIT License",
        "note": "In FFmpeg's Windows build, for NVENC.",
        "texts": ["nvidia-video-codec-headers.txt"],
    },
    {
        "name": "AMD Advanced Media Framework headers (Windows)",
        "license": "MIT License",
        "note": "In FFmpeg's Windows build, for AMF.",
        "texts": ["amd-amf.txt"],
    },
    {
        "name": "Skia",
        "license": "BSD 3-Clause License",
        "note": "Copyright (c) 2011 Google Inc. https://skia.org — the renderer, through rust-skia.",
        "texts": ["skia.txt"],
    },
    {
        "name": "libjpeg-turbo (in Skia)",
        "license": "IJG License, BSD 3-Clause License, zlib License",
        "note": "This software is based in part on the work of the Independent JPEG Group.",
        "texts": ["libjpeg-turbo.md"],
    },
    {
        "name": "libpng (in Skia)",
        "license": "PNG Reference Library License version 2",
        "note": "",
        "texts": ["libpng.txt"],
    },
    {
        "name": "zlib (in Skia)",
        "license": "zlib License",
        "note": "",
        "texts": ["zlib.txt"],
    },
    {
        "name": "Wuffs (in Skia)",
        "license": "MIT License or Apache License 2.0",
        "note": "",
        "texts": ["wuffs.txt"],
    },
    {
        "name": "FreeType (in Skia: Linux and Android builds)",
        "license": "FreeType License",
        "note": (
            "Portions of this software are copyright © 2006-2026 The FreeType "
            "Project (www.freetype.org). All rights reserved."
        ),
        "texts": ["freetype-FTL.txt"],
    },
    {
        "name": "ICU data (in Skia)",
        "license": "Unicode License v3",
        "note": "",
        "texts": ["icu.txt"],
    },
    {
        "name": "Opus (libopus)",
        "license": "BSD 3-Clause License",
        "note": "The voice codec, through opusic-sys. https://opus-codec.org",
        "texts": ["opus.txt"],
    },
    {
        "name": "Lucide icons",
        "license": "ISC License (and MIT for the icons derived from Feather)",
        "note": "https://lucide.dev",
        "texts": ["lucide.txt"],
    },
]


# ── Texts ────────────────────────────────────────────────────────────────


def plain(text: str, markdown: bool) -> str:
    text = text.replace("\r\n", "\n").replace("\t", "    ")
    if markdown:
        text = re.sub(r"\[([^\]]+)\]\(([^)]+)\)", lambda m: m.group(1) if m.group(1) == m.group(2) else f"{m.group(1)} ({m.group(2)})", text)
        text = re.sub(r"^#{1,6}\s*", "", text, flags=re.M)
        text = text.replace("**", "").replace("`", "")
    return text.strip("\n") + "\n"


def paragraphs(text: str) -> list[str]:
    """Hard-wrapped license text as paragraphs the app can wrap itself:
    lines joined, list items kept on their own line, rules dropped."""
    out = []
    for block in re.split(r"\n\s*\n", text.strip("\n")):
        para = ""
        for line in block.split("\n"):
            s = line.strip()
            if not s or re.fullmatch(r"[-=*_~#]{3,}", s):
                continue
            if not para:
                para = s
            elif re.match(r"^([*\-•+]|\(?[a-z0-9]{1,4}[.)])\s", s):
                para += "\n" + s
            else:
                para += " " + s
        if para:
            out.append(para)
    return out


class Texts:
    """Each distinct text once; entries refer to it by index."""

    def __init__(self):
        self.raw: list[str] = []
        self.index: dict[str, int] = {}

    def add(self, text: str) -> int:
        key = re.sub(r"\s+", " ", text).strip()
        if key not in self.index:
            self.index[key] = len(self.raw)
            self.raw.append(text)
        return self.index[key]


def native_text(name: str) -> str:
    path = (NATIVE / name).resolve()
    return plain(path.read_text(), markdown=path.suffix == ".md")


# ── Building the list ────────────────────────────────────────────────────


def crate_label(krate: dict) -> str:
    return f'{krate["name"]} {krate["version"]}'


def build(about: dict):
    texts = Texts()
    slint_like = next(c["crates_like"] for c in COMPONENTS if "crates_like" in c)
    slint_crates = sorted(
        crate_label(c["package"]) for c in about["crates"]
        if re.match(slint_like, c["package"]["name"])
    )

    components = []
    for comp in COMPONENTS:
        note = comp["note"].replace("CRATES", ", ".join(slint_crates))
        blocks = []
        for i, name in enumerate(comp["texts"]):
            blocks.append({"note": note if i == 0 else "", "text": texts.add(native_text(name))})
        if not blocks:
            blocks.append({"note": note, "text": None})
        components.append({"name": comp["name"], "license": comp["license"], "blocks": blocks})

    groups: dict[str, dict] = {}
    crate_count = len(slint_crates)
    seen = set()
    for lic in about["licenses"]:
        users = sorted(
            {crate_label(u["crate"]) for u in lic["used_by"] if u["crate"]["name"] not in OWN}
        )
        if not users:
            continue
        seen.update(users)
        group = groups.setdefault(lic["id"], {"name": lic["name"], "blocks": []})
        group["blocks"].append({"users": users, "text": texts.add(plain(lic["text"], markdown=False))})
    crate_count += len(seen)

    crates = []
    for lic_id, group in groups.items():
        blocks = sorted(group["blocks"], key=lambda b: b["users"][0])
        users = {u for b in blocks for u in b["users"]}
        crates.append({
            "name": group["name"],
            "id": lic_id,
            "count": len(users),
            "blocks": [{"note": ", ".join(b["users"]), "text": b["text"]} for b in blocks],
        })
    crates.sort(key=lambda g: (-g["count"], g["name"]))
    return components, crates, crate_count, texts


def write_app_json(components, crates, crate_count, texts):
    data = {
        "crate_count": crate_count,
        "components": components,
        "crates": [
            {"name": g["name"], "license": "", "count": g["count"], "blocks": g["blocks"]}
            for g in crates
        ],
        "texts": [paragraphs(t) for t in texts.raw],
    }
    APP_JSON.write_text(json.dumps(data, ensure_ascii=False, separators=(",", ":")) + "\n")


def write_site(components, crates, crate_count, texts):
    def entry(name, license_, blocks):
        parts = [f"<details><summary><strong>{html.escape(name)}</strong>"
                 f"<span class=\"lic\">{html.escape(license_)}</span></summary>"]
        for b in blocks:
            if b["note"]:
                parts.append(f'<p class="note">{html.escape(b["note"])}</p>')
            if b["text"] is not None:
                parts.append(f'<pre>{html.escape(texts.raw[b["text"]])}</pre>')
        parts.append("</details>")
        return "\n".join(parts)

    comp_html = "\n".join(entry(c["name"], c["license"], c["blocks"]) for c in components)
    crate_html = "\n".join(
        entry(g["name"], f'{g["count"]} crates', [
            {"note": "Used by: " + b["note"], "text": b["text"]} for b in g["blocks"]
        ])
        for g in crates
    )
    page = SITE_TEMPLATE.replace("{{components}}", comp_html).replace(
        "{{crates}}", crate_html).replace("{{crate_count}}", str(crate_count))
    SITE_HTML.write_text(page)


SITE_TEMPLATE = """<!doctype html>
<!--
  Generated by scripts/licenses.py (CI refreshes it before every deploy,
  .github/workflows/licenses.yml) — do not edit by hand. The same list
  is in the apps, Settings → Licenses.
-->
<html lang="en">
  <head>
    <meta charset="UTF-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1.0" />
    <link rel="icon" type="image/x-icon" href="/img/favicon.ico" />
    <title>baucord — open-source licenses</title>
    <meta
      name="description"
      content="The open-source software baucord is built on, and the license of each part."
    />
    <meta name="theme-color" content="#1a1d21" />
    <style>
      /* The site's "workbench" palette (index.html). */
      :root {
        --paper: #1a1d21;
        --mist: #222529;
        --ink: #d1d2d3;
        --slate: #abadb0;
        --line: rgba(209, 210, 211, 0.12);
        --cobalt-text: #1d9bd1;
        --sans: system-ui, -apple-system, "Segoe UI", Roboto, sans-serif;
        --mono: ui-monospace, "Cascadia Mono", Menlo, monospace;
      }
      * { margin: 0; padding: 0; box-sizing: border-box; }
      body {
        background: var(--paper);
        color: var(--ink);
        font-family: var(--sans);
        font-size: 17px;
        line-height: 1.6;
        -webkit-text-size-adjust: 100%;
      }
      .wrap { max-width: 760px; margin: 0 auto; padding: 0 16px; }
      header { border-bottom: 1px solid var(--line); padding: 18px 0; }
      header a { color: var(--ink); font-weight: 700; text-decoration: none; letter-spacing: 0.01em; }
      main { padding: 40px 0 64px; }
      h1 { font-size: 2rem; line-height: 1.2; margin-bottom: 8px; }
      h2 { font-size: 1.25rem; margin: 40px 0 12px; padding-top: 8px; }
      p { margin-bottom: 12px; }
      a { color: var(--cobalt-text); }
      details { border-bottom: 1px solid var(--line); padding: 10px 0; }
      summary { cursor: pointer; display: flex; flex-wrap: wrap; gap: 4px 12px; align-items: baseline; list-style: none; }
      summary::-webkit-details-marker { display: none; }
      summary::before { content: "\\25B8"; color: var(--slate); width: 0.8em; transition: transform 0.15s; }
      details[open] > summary::before { transform: rotate(90deg); }
      summary .lic { color: var(--slate); font-size: 0.92rem; }
      .note { color: var(--slate); font-size: 0.92rem; margin: 10px 0 8px; overflow-wrap: anywhere; }
      pre {
        font-family: var(--mono);
        font-size: 0.8rem;
        line-height: 1.45;
        white-space: pre-wrap;
        overflow-wrap: anywhere;
        background: var(--mist);
        border: 1px solid var(--line);
        border-radius: 8px;
        padding: 12px 14px;
        margin: 6px 0 14px;
      }
      footer { border-top: 1px solid var(--line); padding: 20px 0 32px; color: var(--slate); font-size: 0.95rem; }
      footer a { margin-right: 16px; }
    </style>
  </head>
  <body>
    <header>
      <div class="wrap"><a href="/">baucord</a></div>
    </header>

    <main class="wrap">
      <h1>Open-source licenses</h1>
      <p>
        baucord is free software under the
        <a href="https://github.com/mpwsh/baucord/blob/main/LICENSE">Apache License 2.0</a>,
        built on the open-source work below. Each part keeps its own
        license; its full text is here, and in the apps under
        Settings → Licenses. The desktop builds include
        FFmpeg and x264, which are under the GNU GPL, so those builds as a
        whole are distributed under the GNU GPL version 3, with their
        complete source on
        <a href="https://github.com/mpwsh/baucord">GitHub</a>.
      </p>

      <h2>Components</h2>
{{components}}

      <h2>Rust crates ({{crate_count}})</h2>
{{crates}}
    </main>

    <footer>
      <div class="wrap">
        <a href="/">baucord</a>
        <a href="terms.html">Terms</a>
        <a href="privacy.html">Privacy</a>
        <a href="licenses.html">Licenses</a>
        <a href="delete-account.html">Delete your account</a>
        <a href="https://github.com/mpwsh/baucord">Source</a>
      </div>
    </footer>
  </body>
</html>
"""


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--about-json", type=Path,
                    help="a `cargo about generate --format json` dump to use instead of running it")
    ap.add_argument("--offline", action="store_true",
                    help="let cargo-about read only local files (crates without a license "
                         "file then get the plain license text, without their copyright line)")
    args = ap.parse_args()

    if args.about_json:
        about = json.loads(args.about_json.read_text())
    else:
        out = subprocess.run(
            ["cargo", "about", "generate", "--format", "json", "--locked"]
            + (["--offline"] if args.offline else []),
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout
        about = json.loads(out)
    components, crates, crate_count, texts = build(about)
    write_app_json(components, crates, crate_count, texts)
    write_site(components, crates, crate_count, texts)
    print(f"{len(components)} components, {crate_count} crates under {len(crates)} licenses, "
          f"{len(texts.raw)} distinct texts → {APP_JSON.relative_to(ROOT)} "
          f"({APP_JSON.stat().st_size // 1024} KB), {SITE_HTML.relative_to(ROOT)} "
          f"({SITE_HTML.stat().st_size // 1024} KB)")


if __name__ == "__main__":
    main()
