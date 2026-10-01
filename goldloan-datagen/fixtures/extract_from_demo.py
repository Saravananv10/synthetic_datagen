#!/usr/bin/env python3
"""Phase 0 — pull the demo's uploaded documents out of the screen recording.

The recording shows the Playground chat UI; each upload is a ~295px-wide
thumbnail in the chat column, and the chat scrolls while the presenter talks.
For each stretch of the recording we grab frames, estimate the scroll between
consecutive frames, and stitch a vertical panorama of the chat column. Each
document is then cropped out of the panorama (so tall ones, like the valuation
certificate, come out whole) and upscaled 3x.

Source resolution is the limit: these are screen captures of thumbnails, good
as fixtures / attachment placeholders, NOT as OCR-grade scans. The pipeline
feeds the agent text summaries (`[DOCUMENT SUMMARY]`, ...) exactly as the
production ingest step does, so pixel quality does not affect trace quality.
Higher-fidelity re-renders can replace these files later without touching
anything else (scenario cards refer to fixtures by id).

Usage:
    python extract_from_demo.py --video "<recording.mp4>" [--out fixtures/demo]
"""
from __future__ import annotations

import argparse
import json
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from PIL import Image, ImageFilter

COL = dict(x=540, y=110, w=340, h=640)   # the chat column inside the 1920x1080 frame
TOP = 140                                 # first row below the sticky header
X0, X1 = 14, 326                          # columns compared when estimating scroll
SCALE = 3

# Stretches of the recording to stitch: name -> (start s, end s)
PANORAMAS = {
    "identity": (259.0, 272.0),
    "gold": (494.0, 505.0),
    "payslips": (696.0, 722.0),
}

# fixture id -> (panorama, (x0, y0, x1, y1) in panorama px, description)
ITEMS = {
    "pan_card": ("identity", (20, 60, 320, 252), "PAN card image (demo sample)"),
    "aadhaar_front": ("identity", (20, 293, 320, 478), "Aadhaar front (demo sample)"),
    "aadhaar_back": ("identity", (20, 478, 320, 662), "Aadhaar back with address (demo sample)"),
    "gold_valuation_certificate": ("gold", (20, 148, 320, 590),
                                   "Certified Gold Valuation Certificate GVC-2026-1157"),
    "payslip_2026_06": ("payslips", (20, 153, 320, 367), "Payslip June 2026"),
    "payslip_2026_04": ("payslips", (20, 405, 320, 805), "Payslip April 2026"),
    "payslip_2026_05": ("payslips", (20, 848, 320, 1046), "Payslip May 2026"),
}
# The necklace photo fits in a single frame.
FRAME_ITEMS = {
    "necklace_on_scale": (497.6, (22, 156, 316, 354), "Necklace on weighing scale, 93.58 g"),
}


def _frames(video: Path, t0: float, t1: float, step: float, out: Path) -> list[Path]:
    out.mkdir(parents=True, exist_ok=True)
    subprocess.run(
        ["ffmpeg", "-nostdin", "-loglevel", "error", "-ss", str(t0), "-t", str(t1 - t0), "-i", str(video),
         "-vf", f"fps=1/{step},crop={COL['w']}:{COL['h']}:{COL['x']}:{COL['y']}", str(out / "%04d.png")],
        check=True,
    )
    return sorted(out.glob("*.png"))


def _scroll(prev: np.ndarray, new: np.ndarray, max_s: int = 420) -> int:
    """Pixels the chat scrolled up between two grayscale frames (0 = no scroll)."""
    p, n = prev[TOP:, X0:X1], new[TOP:, X0:X1]
    h = p.shape[0]
    best, best_s = float("inf"), 0
    for s in range(max_s):
        if h - s < 90:
            break
        err = float(np.mean(np.abs(n[: h - s] - p[s:])))
        if err < best:
            best, best_s = err, s
    return best_s


def build_panorama(video: Path, t0: float, t1: float, tmp: Path, name: str, step: float = 0.4) -> Image.Image:
    frames = _frames(video, t0, t1, step, tmp / name)
    imgs = [np.asarray(Image.open(f).convert("RGB"), dtype=np.float32) for f in frames]
    pano = imgs[0][TOP:].copy()
    for a, b in zip(imgs, imgs[1:]):
        s = _scroll(a.mean(2), b.mean(2))
        if s > 0:
            pano = np.vstack([pano, b[b.shape[0] - s:]])  # rows newly revealed at the bottom
    return Image.fromarray(pano.astype(np.uint8))


def upscale(im: Image.Image) -> Image.Image:
    im = im.resize((im.width * SCALE, im.height * SCALE), Image.LANCZOS)
    return im.filter(ImageFilter.UnsharpMask(radius=2, percent=80, threshold=2))


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--video", required=True, type=Path)
    ap.add_argument("--out", type=Path, default=Path(__file__).parent / "demo")
    ap.add_argument("--save-panoramas", action="store_true", help="also write the stitched panoramas (debug)")
    args = ap.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)

    manifest = {"source": args.video.name, "column_crop": COL, "upscale": SCALE, "items": {}}
    with tempfile.TemporaryDirectory() as td:
        tmp = Path(td)
        panos = {n: build_panorama(args.video, t0, t1, tmp, n) for n, (t0, t1) in PANORAMAS.items()}
        if args.save_panoramas:
            for n, im in panos.items():
                im.save(args.out / f"_pano_{n}.png")
        for name, (pano, box, desc) in ITEMS.items():
            im = upscale(panos[pano].crop(box))
            im.save(args.out / f"{name}.png")
            manifest["items"][name] = {"file": f"{name}.png", "source_range_s": list(PANORAMAS[pano]),
                                       "size": list(im.size), "desc": desc}
        for name, (t, box, desc) in FRAME_ITEMS.items():
            fr = tmp / f"{name}.png"
            subprocess.run(["ffmpeg", "-nostdin", "-loglevel", "error", "-ss", str(t), "-i", str(args.video),
                            "-frames:v", "1", "-vf", f"crop={COL['w']}:{COL['h']}:{COL['x']}:{COL['y']}", str(fr)],
                           check=True)
            im = upscale(Image.open(fr).convert("RGB").crop(box))
            im.save(args.out / f"{name}.png")
            manifest["items"][name] = {"file": f"{name}.png", "source_t_s": t, "size": list(im.size), "desc": desc}
    (args.out / "manifest.json").write_text(json.dumps(manifest, indent=2))
    print(f"wrote {len(manifest['items'])} fixtures to {args.out}")


if __name__ == "__main__":
    main()
