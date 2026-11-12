#!/usr/bin/env python3
"""Compose Pip from parts/ into out/, logo/ and gallery.html. See README.md."""
import argparse
import re
import shutil
import subprocess
import sys
import tomllib
from pathlib import Path

HERE = Path(__file__).parent
PARTS = HERE / "parts"
DEFAULT = {"pose": "stand", "face": "default", "wear": ["tuft"], "scene": "", "bg": "none", "shape": "rounded"}
SHAPES = {
    "rounded": '<rect width="120" height="120" rx="30" fill="{}"/>',
    "circle": '<circle cx="60" cy="60" r="60" fill="{}"/>',
    "square": '<rect width="120" height="120" fill="{}"/>',
}
GALLERY = """<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Hako</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Fredoka:wght@500;600&display=swap" rel="stylesheet">
<style>
:root{--bg:#0e0f11;--text:#f4f5f7;--muted:#9ea1ab;--line:#2e3036;--raise:#17181b;--accent:#38bdf8}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--text);font:16px/1.55 system-ui,-apple-system,"Segoe UI",sans-serif}
main{max-width:960px;margin:0 auto;padding:28px 16px 64px}
h1{font:600 clamp(2rem,7vw,3rem)/1.08 Fredoka,system-ui,sans-serif;margin:0 0 12px}
h2{font:600 clamp(1.5rem,5vw,1.9rem)/1.15 Fredoka,system-ui,sans-serif;margin:0 0 6px}
.note{max-width:62ch;color:var(--muted);margin:0 0 8px}
section{margin-top:44px;padding-top:26px;border-top:1px solid var(--line)}
.grid{display:grid;grid-template-columns:repeat(auto-fill,minmax(128px,1fr));gap:22px 16px;margin-top:20px}
figure{margin:0}
figure svg{display:block;width:100%;height:auto}
figure.tint svg{background:var(--tint,#FFC94D);border-radius:25%}
figcaption{margin-top:8px;font:.9rem ui-monospace,SFMono-Regular,Menlo,monospace;color:var(--muted);overflow-wrap:anywhere}
.pick{display:inline-flex;align-items:center;gap:12px;min-height:44px;margin-top:8px}
.pick input{width:52px;height:44px;padding:0;border:1px solid var(--line);border-radius:10px;background:none;cursor:pointer}
.pick input:focus-visible{outline:3px solid var(--accent);outline-offset:2px}
.logos{grid-template-columns:repeat(auto-fill,minmax(240px,1fr))}
.sw{display:grid;place-items:center;height:128px;padding:20px;border-radius:14px;background:#f4f5f7;color:#1E293B}
.dark .sw{background:var(--raise);color:#F8FAFC;border:1px solid var(--line)}
.sw svg{width:auto;max-width:100%;height:100%}
pre{background:var(--raise);border:1px solid var(--line);border-radius:10px;padding:14px 16px;overflow-x:auto;font:.9rem/1.6 ui-monospace,SFMono-Regular,Menlo,monospace;margin:16px 0 0}
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.92em;background:var(--raise);padding:1px 6px;border-radius:6px}
pre code{background:none;padding:0;font-size:1em}
</style>
</head>
<body>
<main>
<h1>Hako</h1>
<p class="note">The logo, and every ready-made version of Pip the mascot. Mascot labels are file names in <code>out/</code>: <code>pip-&lt;name&gt;.svg</code>, with a PNG beside each.</p>
<section>
<h2>Logo</h2>
<p class="note">No background. The first two are for light surfaces, the outlined one for dark ones, and the glyph takes whatever text colour surrounds it.</p>
<div class="grid logos">
%LOGOS%</div>
</section>
<section>
<h2>Mascot</h2>
<label class="pick">Colour behind the transparent ones <input type="color" id="bg" value="#FFC94D"></label>
</section>
%SECTIONS%
<section>
<h2>Making a new one</h2>
<p class="note">Pip is stored as layers: body, pose, face, things to wear and background scenes. A version is a few lines in <code>presets.toml</code>, or a one-off command.</p>
<pre><code># presets.toml
[events.christmas]
face  = "happy"
wear  = ["hat-santa"]
scene = "snow"
bg    = "#1F7A5C"</code></pre>
<pre><code>./build.py --png 512     # rebuild everything, this page included
./build.py base --bg '#0EA5E9' --shape circle -o pip-blue.svg</code></pre>
<p class="note" style="margin-top:16px">For a new event, draw the hat or prop over <code>pip-base-plain</code>, save it into <code>parts/wear/</code> and add a preset. The steps are in <code>README.md</code>.</p>
</section>
</main>
<script>
var bg=document.getElementById('bg');
bg.addEventListener('input',function(){document.querySelectorAll('.tint').forEach(function(f){f.style.setProperty('--tint',bg.value);});});
</script>
</body>
</html>
"""


def part(kind, name, layer=""):
    """Inner markup of parts/<kind>/<name><layer>.svg, or "" if there is no such file."""
    f = PARTS / kind / f"{name}{layer}.svg"
    return re.search(r"<svg[^>]*>(.*)</svg>", f.read_text(), re.S).group(1).strip() if f.exists() else ""


def svg(box, body):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="{box}">\n{body}\n</svg>\n'


def compose(p):
    return svg("0 0 120 120", stack(p))


def stack(p):
    """The preset's layers, bottom to top, without the <svg> wrapper."""
    for kind, names in (("pose", [p["pose"]]), ("face", [p["face"]]), ("scene", [p["scene"]]), ("wear", p["wear"])):
        for n in filter(None, names):
            if not part(kind, n) and not part(kind, n, ".back"):
                have = sorted({f.name.split(".")[0] for f in (PARTS / kind).glob("*.svg")})
                sys.exit(f"no {kind} called {n!r}; have: {', '.join(have)}")
    bg = p["bg"] != "none"
    layers = []
    if bg:
        if not re.fullmatch(r"#[0-9a-fA-F]{6}", p["bg"]):
            sys.exit(f"bg must be 'none' or a #rrggbb colour, got {p['bg']!r}")
        if p["shape"] not in SHAPES:
            sys.exit(f"no shape called {p['shape']!r}; have: {', '.join(SHAPES)}")
        r, g, b = (int(p["bg"][i:i + 2], 16) for i in (1, 3, 5))
        dark = 0.299 * r + 0.587 * g + 0.114 * b < 110
        # the lighter disc behind Pip; stronger on dark colours so the dark body still reads
        layers += [SHAPES[p["shape"]].format(p["bg"]),
                   f'<circle cx="60" cy="62" r="46" fill="#fff" opacity="{0.2 if dark else 0.25}"/>']
    if p["scene"]:
        layers.append(part("scene", p["scene"]))
    if bg:
        layers.append('<ellipse cx="60" cy="104" rx="27" ry="4" opacity=".18"/>')
    layers.append(part("pose", p["pose"]))
    layers += [part("wear", w, ".back") for w in p["wear"]]
    layers += [part("", "body"), part("face", p["face"])]
    layers += [part("wear", w) for w in p["wear"]]
    return "\n".join(filter(None, layers))


def logos():
    """logo/: Pip and the wordmark with no background, cropped tight."""
    mark, lockup = "17 19 86 88", "15 17 276 92"
    pip, word = stack(DEFAULT), part("", "wordmark")
    files = {
        "hako-mark": svg(mark, pip),
        "hako-logo": svg(lockup, pip + "\n" + word.replace("currentColor", "#1E293B")),
        "hako-logo-dark": svg(lockup, "\n".join((part("", "outline"), pip, word.replace("currentColor", "#F8FAFC")))),
        "hako-glyph": svg(mark, part("", "glyph")),
    }
    (HERE / "logo").mkdir(exist_ok=True)
    for name, text in files.items():
        (HERE / "logo" / f"{name}.svg").write_text(text)
    return files


def gallery(groups, logo_files):
    tiles = ""
    for n, text in logo_files.items():
        dark = ' class="dark"' if "dark" in n else ""
        tiles += f'<figure{dark}><div class="sw">{text}</div><figcaption>logo/{n}.svg</figcaption></figure>\n'
    page = GALLERY.replace("%LOGOS%", tiles)
    sections = ""
    for group, presets in groups.items():
        tiles = ""
        for n, p in presets.items():
            svg = compose({**DEFAULT, **p}).replace("<svg ", f'<svg role="img" aria-label="Pip, {n}" ', 1)
            tint = ' class="tint"' if p.get("bg", "none") == "none" else ""
            tiles += f"<figure{tint}>{svg}<figcaption>{n}</figcaption></figure>\n"
        sections += f'<section>\n<h2>{group.capitalize()}</h2>\n<div class="grid">\n{tiles}</div>\n</section>\n'
    (HERE / "gallery.html").write_text(page.replace("%SECTIONS%", sections))


def png(svg, size):
    chrome = next(filter(None, map(shutil.which, ("google-chrome", "chromium", "chromium-browser"))), None)
    if not chrome:
        sys.exit("--png needs Chrome or Chromium on PATH")
    subprocess.run([chrome, "--headless=new", "--disable-gpu", "--hide-scrollbars",
                    "--default-background-color=00000000", f"--window-size={size},{size}",
                    f"--screenshot={svg.with_suffix('.png').resolve()}", svg.resolve().as_uri()],
                   check=True, capture_output=True)


def main():
    groups = tomllib.loads((HERE / "presets.toml").read_text())
    presets = {n: p for group in groups.values() for n, p in group.items()}
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("presets", nargs="*", help="preset names from presets.toml (default: all)")
    for key in ("pose", "face", "scene", "bg", "shape"):
        ap.add_argument(f"--{key}", help=f"override the preset's {key}")
    ap.add_argument("--wear", help="comma-separated list that replaces the preset's wear ('' for nothing)")
    ap.add_argument("-o", "--output", type=Path, help="write one preset to this file instead of out/")
    ap.add_argument("--png", type=int, metavar="SIZE", help="also render a SIZE px PNG next to each SVG")
    a = ap.parse_args()

    names = a.presets or list(presets)
    if a.output and len(names) != 1:
        ap.error("-o needs exactly one preset")
    over = {k: v for k in ("pose", "face", "scene", "bg", "shape") if (v := getattr(a, k)) is not None}
    if a.wear is not None:
        over["wear"] = [w for w in a.wear.split(",") if w]
    (HERE / "out").mkdir(exist_ok=True)
    for n in names:
        if n not in presets:
            ap.error(f"no preset called {n!r}; have: {', '.join(presets)}")
        svg = a.output or HERE / "out" / f"pip-{n}.svg"
        svg.write_text(compose({**DEFAULT, **presets[n], **over}))
        if a.png:
            png(svg, a.png)
        print(svg)
    if not (a.presets or a.output or over):
        gallery(groups, logos())
        print(HERE / "logo")
        print(HERE / "gallery.html")


if __name__ == "__main__":
    main()
