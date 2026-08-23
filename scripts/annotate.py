#!/usr/bin/env python3
"""Local web UI for placing ground-truth frame rectangles on the thumbnail
corpus, saved to thumbnails/ground_truth.json.

    python3 scripts/annotate.py

then open http://localhost:8756. Click the strip to drop a frame at that
column (width is fixed to the format's frame length, same as detect()
would use); drag a frame to move it, click its x to remove it. "Show
detect()" overlays what boundaries::detect currently reports, in red, for
comparison - it shells out to `cargo run --example detect_frames`, so the
first request after a source change rebuilds it.

Stdlib only. Needs `magick` (ImageMagick) on PATH to render the previews and
a built `nkscan` checkout to compare against.
"""

import json
import re
import subprocess
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse

ROOT = Path(__file__).resolve().parent.parent
CORPUS = ROOT / "thumbnails"
MANIFEST = CORPUS / "ground_truth.json"
PREVIEW_CACHE = ROOT / "target" / "annotate-previews"
PORT = 8756

# Frame height in tenths of a millimeter, mirroring FilmFormat::height_tenths
# (src/protocol/caps/film.rs) - kept in sync by hand, there are only nine of
# these and they don't change
FORMAT_TENTHS = {
    "ix240": 302,
    "f135": 360,
    "f135half": 180,
    "f16": 200,
    "f645": 415,
    "f66": 560,
    "f67": 695,
    "f68": 760,
    "f69": 840,
}


def height_dots(fmt: str, dpi: float) -> int:
    tenths = FORMAT_TENTHS.get(fmt)
    if tenths is None:
        return 0
    return round(tenths * dpi / 254)


def guess_format(name: str) -> str:
    if name.startswith("35mm"):
        return "f135"
    if name.startswith("6x45"):
        return "f645"
    if name.startswith("6x9"):
        return "f69"
    # 6x6 and holga (holga bodies shoot 6x6 in this corpus)
    return "f66"


def guess_polarity(name: str) -> str:
    return "positive" if "slide" in name else "negative"


def identify(path: Path) -> tuple[int, int, float]:
    out = subprocess.run(
        ["magick", "identify", "-units", "PixelsPerInch", "-format", "%w %h %x", str(path)],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    w, h, dpi = out.split()
    return int(w), int(h), float(dpi) or 97.0


def load_manifest() -> dict:
    if MANIFEST.exists():
        return json.loads(MANIFEST.read_text())
    return {"_readme": "Manually-verified frame placements. Edit with scripts/annotate.py."}


def save_manifest(data: dict) -> None:
    MANIFEST.write_text(json.dumps(data, indent=2) + "\n")


def preview_png(name: str) -> bytes:
    PREVIEW_CACHE.mkdir(parents=True, exist_ok=True)
    src = CORPUS / name
    cached = PREVIEW_CACHE / (name + ".png")
    if not cached.exists() or cached.stat().st_mtime < src.stat().st_mtime:
        subprocess.run(
            # Native resolution: the page stretches height with CSS, adjustably,
            # rather than baking in a fixed guess here. -auto-level first, on
            # the raw linear sensor values, then -gamma encodes for display -
            # these TIFFs carry no gamma/TRC tag, so skipping this renders
            # linear data as if it were already sRGB and everything looks
            # far too dark. The textbook display gamma (2.2) puts ~18% of
            # pixels above half-scale on this corpus - washed out, not just
            # brighter - because these scans don't fill their nominal range
            # the way a properly-exposed photo would; 1.4 lands ~2%, a normal
            # -looking spread, while still gamma-correcting rather than
            # leaving the data linear
            ["magick", str(src), "-auto-level", "-gamma", "1.4", str(cached)],
            check=True,
        )
    return cached.read_bytes()


def run_detect(name: str, fmt: str, polarity: str) -> list[int]:
    """Shell out to the existing example and parse its stderr for columns"""
    out_tmp = PREVIEW_CACHE / "detect-scratch.tiff"
    PREVIEW_CACHE.mkdir(parents=True, exist_ok=True)
    proc = subprocess.run(
        [
            "cargo", "run", "-q", "--example", "detect_frames", "--",
            str(CORPUS / name), "--format", fmt, "--polarity", polarity,
            "-o", str(out_tmp),
        ],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    return [int(m) for m in re.findall(r"columns \[(\d+),", proc.stderr)]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, fmt, *args):
        pass

    def _json(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        url = urlparse(self.path)
        if url.path == "/":
            body = PAGE.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        if url.path == "/api/list":
            manifest = load_manifest()
            files = []
            for p in sorted(CORPUS.glob("*.tiff")):
                entry = manifest.get(p.name, {})
                w, h, dpi = identify(p)
                fmt = entry.get("format") or guess_format(p.name)
                polarity = entry.get("polarity") or guess_polarity(p.name)
                default_length = height_dots(fmt, dpi)
                files.append({
                    "name": p.name,
                    "width": w,
                    "height": h,
                    "dpi": dpi,
                    "format": fmt,
                    "polarity": polarity,
                    # The format's nominal length; entry["length"], if set,
                    # overrides it for this file - real cameras don't all
                    # advance exactly the format's millimeter figure
                    "defaultLength": default_length,
                    "length": entry.get("length", default_length),
                    "frames": entry.get("frames", []),
                    "note": entry.get("note"),
                })
            self._json({"files": files})
            return

        if url.path.startswith("/api/image/"):
            name = url.path[len("/api/image/"):]
            try:
                png = preview_png(name)
            except subprocess.CalledProcessError as e:
                self._json({"error": str(e)}, 500)
                return
            self.send_response(200)
            self.send_header("Content-Type", "image/png")
            self.send_header("Content-Length", str(len(png)))
            self.end_headers()
            self.wfile.write(png)
            return

        if url.path == "/api/detect":
            q = parse_qs(url.query)
            name, fmt, polarity = q["name"][0], q["format"][0], q["polarity"][0]
            self._json({"frames": run_detect(name, fmt, polarity)})
            return

        self.send_response(404)
        self.end_headers()

    def do_POST(self):
        if self.path == "/api/save":
            content_length = int(self.headers.get("Content-Length", 0))
            body = json.loads(self.rfile.read(content_length))
            manifest = load_manifest()
            existing = manifest.get(body["name"], {})
            existing["format"] = body["format"]
            existing["polarity"] = body["polarity"]
            existing["frames"] = sorted(body["frames"])
            _, _, dpi = identify(CORPUS / body["name"])
            default_length = height_dots(body["format"], dpi)
            override = body.get("length")
            if override is not None and override != default_length:
                existing["length"] = override
            else:
                existing.pop("length", None)
            manifest[body["name"]] = existing
            save_manifest(manifest)
            self._json({"ok": True})
            return
        self.send_response(404)
        self.end_headers()


PAGE = r"""<!doctype html>
<meta charset="utf-8">
<title>frame annotator</title>
<style>
  :root { color-scheme: dark; }
  body { margin: 0; display: flex; height: 100vh; font: 13px system-ui, sans-serif; background: #1b1b1f; color: #ddd; }
  #sidebar { width: 260px; overflow-y: auto; border-right: 1px solid #333; flex: none; }
  #sidebar div.item { padding: 6px 10px; cursor: pointer; border-bottom: 1px solid #26262b; }
  #sidebar div.item:hover { background: #26262b; }
  #sidebar div.item.active { background: #33415c; }
  #sidebar .note { color: #e0a030; font-size: 11px; }
  #main { flex: 1; display: flex; flex-direction: column; min-width: 0; }
  #toolbar { padding: 8px 12px; border-bottom: 1px solid #333; display: flex; gap: 12px; align-items: center; flex-wrap: wrap; }
  #canvasWrap { flex: 1; overflow: auto; position: relative; background: #000; }
  #stage { position: relative; display: inline-block; }
  #stage img { display: block; }
  .frame { position: absolute; top: 0; bottom: 0; border: 2px solid #4ade80; cursor: grab; }
  .frame .x { position: absolute; top: 2px; right: 2px; width: 16px; height: 16px; background: #222; color: #fff; text-align: center; line-height: 16px; border-radius: 3px; cursor: pointer; font-size: 11px; }
  .frame .label { position: absolute; bottom: 2px; left: 2px; background: #000a; padding: 1px 4px; font-size: 11px; }
  .detected { position: absolute; top: 0; bottom: 0; border: 2px dashed #f87171; pointer-events: none; }
  .detected .label { position: absolute; top: 2px; left: 2px; background: #000a; color: #f87171; padding: 1px 4px; font-size: 11px; }
  select, button, input[type=number] { background: #26262b; color: #ddd; border: 1px solid #444; border-radius: 4px; padding: 4px 8px; }
  button.primary { background: #2d6a4f; border-color: #2d6a4f; }
  #ruler { height: 16px; position: relative; background: #111; font-size: 10px; color: #888; }
</style>
<body>
<div id="sidebar"></div>
<div id="main">
  <div id="toolbar">
    <span id="title">-</span>
    format <select id="format"><option>f135</option><option>f645</option><option>f66</option><option>f69</option><option>ix240</option><option>f135half</option><option>f16</option><option>f67</option><option>f68</option></select>
    polarity <select id="polarity"><option>negative</option><option>positive</option></select>
    frame width <input id="lengthSlider" type="range" min="10" max="10" step="1" value="10">
    <span id="length">-</span>px
    <button id="resetLength" title="back to the format's nominal length at this file's dpi">reset</button>
    zoom <input id="zoom" type="range" min="0.2" max="10" step="0.1" value="4">
    <span id="zoomVal">4x</span>
    <label><input id="invert" type="checkbox"> invert</label>
    <button id="autoWb">auto white balance</button>
    <button id="resetWb">reset WB</button>
    <button id="pickBase" title="click a gap between frames (or the border) afterward - that pixel becomes true black">pick film base as black</button>
    <button id="pickWb" title="click a point that should be neutral (gray/white) afterward - its color cast is removed, its own brightness kept">pick white balance point</button>
    <button id="showDetect">show detect()</button>
    <button id="startFromDetect" title="replaces the editable frames below with detect()'s current output, as a draft to correct against the image - not a verified placement">start from detect()</button>
    <button id="save" class="primary">save</button>
    <span id="status"></span>
  </div>
  <div id="canvasWrap"><div id="stage"><img id="img"><div id="ruler"></div></div></div>
</div>
<svg width="0" height="0" style="position:absolute">
  <filter id="wb" color-interpolation-filters="sRGB">
    <feComponentTransfer>
      <feFuncR id="wbR" type="linear" slope="1"/>
      <feFuncG id="wbG" type="linear" slope="1"/>
      <feFuncB id="wbB" type="linear" slope="1"/>
    </feComponentTransfer>
  </filter>
</svg>
<script>
let files = [], current = null, frames = [], detected = null, drag = null, zoom = 4;

// Mirrors FORMAT_TENTHS in this script / FilmFormat::height_tenths in Rust
const FORMAT_TENTHS = { ix240: 302, f135: 360, f135half: 180, f16: 200, f645: 415, f66: 560, f67: 695, f68: 760, f69: 840 };
function heightDots(fmt, dpi) { return Math.round(FORMAT_TENTHS[fmt] * dpi / 254); }

async function api(path, opts) { const r = await fetch(path, opts); return r.json(); }

async function loadList() {
  const data = await api('/api/list');
  files = data.files;
  const bar = document.getElementById('sidebar');
  bar.innerHTML = '';
  for (const f of files) {
    const d = document.createElement('div');
    d.className = 'item';
    d.textContent = f.name + ' (' + f.frames.length + ')';
    if (f.note) { const n = document.createElement('div'); n.className='note'; n.textContent='has note'; d.appendChild(n); }
    d.onclick = () => select(f.name);
    d.dataset.name = f.name;
    bar.appendChild(d);
  }
  if (!current && files.length) select(files[0].name);
}

async function fetchDetect() {
  const r = await api('/api/detect?name=' + encodeURIComponent(current.name) + '&format=' + current.format + '&polarity=' + current.polarity);
  return r.frames;
}

async function select(name) {
  current = files.find(f => f.name === name);
  frames = [...current.frames];
  detected = null;
  setPickMode(null);
  document.getElementById('showDetect').textContent = 'show detect()';
  document.querySelectorAll('#sidebar .item').forEach(d => d.classList.toggle('active', d.dataset.name === name));
  document.getElementById('title').textContent = name;
  document.getElementById('format').value = current.format;
  document.getElementById('polarity').value = current.polarity;
  syncLengthSlider();
  const img = document.getElementById('img');
  img.src = '/api/image/' + name;
  applyZoom();
  resetWb();
  // A negative reads as a positive image inverted - default to that view,
  // still overridable per file
  document.getElementById('invert').checked = current.polarity === 'negative';
  applyFilters();
  document.getElementById('status').textContent = '';
  render();
  // Nothing placed yet for this file - seed a draft from detect() rather
  // than starting blank, since most of the corpus is already close. Still
  // just a draft: check it against the image before saving, this is
  // exactly the placement that might be wrong
  if (frames.length === 0) {
    document.getElementById('status').textContent = 'seeding draft from detect()...';
    frames = await fetchDetect();
    document.getElementById('status').textContent = 'draft from detect() - verify against the image before saving';
    render();
  }
}

function currentLength() { return current.length; }

// Range is the nominal length +/-50% - real cameras don't all advance
// exactly the format's millimeter figure, so the slider needs headroom
// past "nominal" in both directions, not just down to allow for overlap
function syncLengthSlider() {
  const slider = document.getElementById('lengthSlider');
  slider.min = Math.max(1, Math.round(current.defaultLength * 0.5));
  slider.max = Math.round(current.defaultLength * 1.5);
  slider.value = current.length;
  document.getElementById('length').textContent = current.length;
}

// Everything - image, frame boxes, ruler ticks - is authored in native
// column units and scaled by `zoom` in both dimensions here, so a frame's
// true column is always (displayed px) / zoom, not a fixed 1:1 mapping
function applyZoom() {
  if (!current) return;
  const img = document.getElementById('img');
  img.style.width = (current.width * zoom) + 'px';
  img.style.height = (current.height * zoom) + 'px';
}

// Chains the white-balance SVG filter (always present, identity when its
// slopes are 1) with invert - CSS filter accepts a space-separated list of
// both url() references and named functions together
function applyFilters() {
  const img = document.getElementById('img');
  const invert = document.getElementById('invert').checked;
  const value = 'url(#wb)' + (invert ? ' invert(1)' : '');
  // Some engines cache the referenced SVG filter's output and won't repaint
  // just because a <feFuncR> slope changed underneath an already-applied
  // filter: url(#wb) - force one by clearing it, forcing layout, then
  // reapplying, rather than trust a same-value reassignment to invalidate it
  img.style.filter = 'none';
  void img.offsetHeight;
  img.style.filter = value;
}

// feFunc's `type="linear"` computes slope*C + intercept. Every caller sets
// both explicitly, even to their identity (1, 0) - leaving one attribute
// untouched would let a previous mode's value leak into this one, e.g. a
// black-point intercept surviving into a later pure-gain white-balance
function setGain(slope, intercept) {
  const ids = ['wbR', 'wbG', 'wbB'];
  for (let i = 0; i < 3; i++) {
    const el = document.getElementById(ids[i]);
    el.setAttribute('slope', slope[i].toFixed(4));
    el.setAttribute('intercept', intercept[i].toFixed(4));
  }
  applyFilters();
}

function resetWb() {
  setGain([1, 1, 1], [0, 0, 0]);
}

// Gray-world auto white balance: sample the displayed (already gamma-
// corrected) image into an offscreen canvas, then gain each channel so its
// mean matches the average of all three - a rough visual aid for judging
// content through a color-neg mask, not a color-accurate correction
function autoWb() {
  const img = document.getElementById('img');
  const canvas = document.createElement('canvas');
  canvas.width = current.width;
  canvas.height = current.height;
  const ctx = canvas.getContext('2d');
  ctx.drawImage(img, 0, 0, current.width, current.height);
  const data = ctx.getImageData(0, 0, current.width, current.height).data;
  let sum = [0, 0, 0];
  const n = data.length / 4;
  for (let i = 0; i < data.length; i += 4) {
    sum[0] += data[i]; sum[1] += data[i + 1]; sum[2] += data[i + 2];
  }
  const mean = sum.map(s => s / n);
  const target = (mean[0] + mean[1] + mean[2]) / 3;
  const gain = mean.map(m => m > 1 ? target / m : 1);
  setGain(gain, [0, 0, 0]);
}

function render() {
  const stage = document.getElementById('stage');
  stage.querySelectorAll('.frame,.detected').forEach(e => e.remove());
  const len = currentLength();
  frames.forEach((x, i) => {
    const el = document.createElement('div');
    el.className = 'frame';
    el.style.left = (x * zoom) + 'px';
    el.style.width = (len * zoom) + 'px';
    el.innerHTML = '<div class="label">' + x + '</div><div class="x">x</div>';
    el.querySelector('.x').onclick = (e) => { e.stopPropagation(); frames.splice(i, 1); render(); };
    el.onmousedown = (e) => { if (e.target.classList.contains('x')) return; drag = { i, startX: e.clientX, orig: x, el }; e.preventDefault(); };
    stage.insertBefore(el, document.getElementById('ruler'));
  });
  if (detected) {
    detected.forEach(x => {
      const el = document.createElement('div');
      el.className = 'detected';
      el.style.left = (x * zoom) + 'px';
      el.style.width = (len * zoom) + 'px';
      el.innerHTML = '<div class="label">detect ' + x + '</div>';
      stage.insertBefore(el, document.getElementById('ruler'));
    });
  }
  const ruler = document.getElementById('ruler');
  ruler.innerHTML = '';
  ruler.style.width = (current.width * zoom) + 'px';
  for (let x = 0; x < current.width; x += 50) {
    const t = document.createElement('span');
    t.style.position = 'absolute'; t.style.left = (x * zoom) + 'px'; t.textContent = x;
    ruler.appendChild(t);
  }
}

// Bound to window, not #stage: #stage is exactly image-sized, and these
// thumbnails are short enough that a horizontal drag easily drifts a few
// px outside its box, which would otherwise stop the drag dead. Mutates
// the dragged element directly rather than calling render(), which was
// tearing the element out from under the drag on every tick
window.addEventListener('mousemove', (e) => {
  if (!drag) return;
  const dx = (e.clientX - drag.startX) / zoom;
  let x = Math.max(0, Math.min(current.width - currentLength(), drag.orig + dx));
  x = Math.round(x);
  frames[drag.i] = x;
  drag.el.style.left = (x * zoom) + 'px';
  drag.el.querySelector('.label').textContent = x;
});
window.addEventListener('mouseup', () => { drag = null; });

// null | 'base' | 'wb' - which eyedropper, if either, the next image click feeds
let pickMode = null;
const PICKERS = {
  base: { btn: 'pickBase', label: 'pick film base as black', prompt: 'click a gap/border pixel in the image' },
  wb: { btn: 'pickWb', label: 'pick white balance point', prompt: 'click a point that should be neutral gray/white' },
};

document.getElementById('img').addEventListener('click', (e) => {
  const rect = e.target.getBoundingClientRect();
  if (pickMode) {
    // Unclamped - the frame-placement clamp below keeps a frame's left
    // edge from running off the image, which has nothing to do with which
    // pixel got clicked and was cutting picks short of the right edge
    const px = Math.max(0, Math.min(current.width - 1, Math.round((e.clientX - rect.left) / zoom)));
    const py = Math.max(0, Math.min(current.height - 1, Math.round((e.clientY - rect.top) / zoom)));
    pickAt(px, py);
    return;
  }
  const x = Math.max(0, Math.min(current.width - currentLength(), (e.clientX - rect.left) / zoom));
  frames.push(Math.round(x));
  render();
});

// Shared by both eyedroppers: sample the clicked pixel, then set a per-
// channel gain against it. 'base' forces it to full white pre-invert (0,0,0
// once inverted) - real content near that level (highlights, the bare gate)
// clips along with it, the same tradeoff a film-base eyedropper makes in
// scan software. 'wb' instead targets that pixel's own average brightness,
// so it becomes neutral without forcing it to black or white
function pickAt(x, y) {
  const img = document.getElementById('img');
  const canvas = document.createElement('canvas');
  canvas.width = current.width;
  canvas.height = current.height;
  const ctx = canvas.getContext('2d');
  ctx.drawImage(img, 0, 0, current.width, current.height);
  const sample = [...ctx.getImageData(x, y, 1, 1).data].slice(0, 3);
  const invertOn = document.getElementById('invert').checked;
  if (pickMode === 'base' && !invertOn) {
    // No invert downstream to flip a "push toward white" into black, so
    // shift directly: slope 1, intercept -sample - a pure gain here is
    // degenerate (target 0 means slope = 0/v = 0 for every pixel, crushing
    // the whole image instead of just the picked point)
    setGain([1, 1, 1], sample.map(v => -(v / 255)));
  } else {
    // 'base' with invert on: push toward white pre-invert, which becomes
    // black once invert(1) runs. 'wb': push toward this pixel's own
    // average, neutralizing cast without forcing black or white
    const target = pickMode === 'base' ? 255 : (sample[0] + sample[1] + sample[2]) / 3;
    setGain(sample.map(v => v > 1 ? target / v : 1), [0, 0, 0]);
  }
  const [r, g, b] = sample;
  const p = PICKERS[pickMode];
  document.getElementById('status').textContent = `${p.label} set from (${x}, ${y}): was rgb(${r},${g},${b})`;
  setPickMode(null);
}

function setPickMode(mode) {
  if (pickMode) document.getElementById(PICKERS[pickMode].btn).textContent = PICKERS[pickMode].label;
  pickMode = pickMode === mode ? null : mode;
  document.getElementById('img').style.cursor = pickMode ? 'crosshair' : '';
  if (pickMode) {
    document.getElementById(PICKERS[pickMode].btn).textContent = 'click in the image...';
    document.getElementById('status').textContent = PICKERS[pickMode].prompt;
  }
}

document.getElementById('pickBase').onclick = () => setPickMode('base');
document.getElementById('pickWb').onclick = () => setPickMode('wb');

document.getElementById('zoom').oninput = (e) => {
  zoom = parseFloat(e.target.value);
  document.getElementById('zoomVal').textContent = zoom + 'x';
  applyZoom();
  render();
};

document.getElementById('invert').onchange = applyFilters;
document.getElementById('autoWb').onclick = autoWb;
document.getElementById('resetWb').onclick = () => { resetWb(); };

document.getElementById('lengthSlider').oninput = (e) => {
  current.length = parseInt(e.target.value, 10);
  document.getElementById('length').textContent = current.length;
  render();
};
document.getElementById('resetLength').onclick = () => {
  current.length = current.defaultLength;
  syncLengthSlider();
  render();
};

document.getElementById('format').onchange = (e) => {
  current.format = e.target.value;
  current.defaultLength = heightDots(current.format, current.dpi);
  current.length = current.defaultLength;
  syncLengthSlider();
  render();
};
document.getElementById('polarity').onchange = (e) => {
  current.polarity = e.target.value;
  document.getElementById('invert').checked = current.polarity === 'negative';
  applyFilters();
};

document.getElementById('showDetect').onclick = async () => {
  const btn = document.getElementById('showDetect');
  if (detected) {
    detected = null;
    btn.textContent = 'show detect()';
    render();
    return;
  }
  document.getElementById('status').textContent = 'running detect()...';
  detected = await fetchDetect();
  btn.textContent = 'hide detect()';
  document.getElementById('status').textContent = '';
  render();
};

document.getElementById('startFromDetect').onclick = async () => {
  document.getElementById('status').textContent = 'running detect()...';
  frames = await fetchDetect();
  document.getElementById('status').textContent = 'draft from detect() - verify against the image before saving';
  render();
};

document.getElementById('save').onclick = async () => {
  await api('/api/save', {
    method: 'POST',
    body: JSON.stringify({ name: current.name, format: current.format, polarity: current.polarity, frames, length: current.length }),
  });
  current.frames = [...frames];
  document.getElementById('status').textContent = 'saved';
  loadList();
};

loadList();
</script>
"""


def main():
    server = ThreadingHTTPServer(("localhost", PORT), Handler)
    print(f"annotator on http://localhost:{PORT}  (Ctrl-C to stop)", file=sys.stderr)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
