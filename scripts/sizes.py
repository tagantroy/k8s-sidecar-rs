#!/usr/bin/env python3
"""Measure the binary and image size of a built image, report them, and draw badges.

    scripts/sizes.py IMAGE [--previous sizes.json] [--summary FILE] [--badges DIR]

- binary: /usr/local/bin/k8s-sidecar copied out of the image (what actually ships).
- image:  `docker save | gzip`, which is within ~1% of the compressed size in the registry
          (what users download). The uncompressed size is reported too.
- --previous: sizes.json from the last run on main; the report shows the change.
- --summary:  append a Markdown table (e.g. $GITHUB_STEP_SUMMARY).
- --badges:   write binary.svg, image.svg and sizes.json into DIR.
Needs only Docker and the Python standard library.
"""

from __future__ import annotations

import argparse
import gzip
import json
import os
import subprocess
import tempfile

BINARY_PATH = "/usr/local/bin/k8s-sidecar"


def docker(*args: str) -> str:
    return subprocess.run(["docker", *args], check=True, capture_output=True, text=True).stdout.strip()


def measure(image: str) -> dict:
    container = docker("create", image)
    try:
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "bin")
            docker("cp", f"{container}:{BINARY_PATH}", path)
            binary = os.path.getsize(path)
    finally:
        docker("rm", container)
    save = subprocess.Popen(["docker", "save", image], stdout=subprocess.PIPE)
    compressed = 0
    with tempfile.TemporaryFile() as sink:
        with gzip.GzipFile(fileobj=sink, mode="wb", compresslevel=6) as gz:
            while chunk := save.stdout.read(1 << 20):
                gz.write(chunk)
        compressed = sink.tell()
    if save.wait() != 0:
        raise SystemExit("docker save failed")
    uncompressed = int(docker("image", "inspect", image, "--format", "{{.Size}}"))
    return {"binary_bytes": binary, "image_bytes": compressed, "image_uncompressed_bytes": uncompressed}


def mb(n: int) -> str:
    return f"{n / 1e6:.1f} MB"


def change(new: int, old: int | None) -> str:
    if not old:
        return "–"
    d = new - old
    if abs(d) < 10_000:  # under 10 kB is noise (gzip, timestamps)
        return "no change"
    return f"{'+' if d > 0 else '−'}{abs(d) / 1e6:.2f} MB ({d / old:+.1%})"


def badge(label: str, value: str) -> str:
    """A flat badge in the usual style, sized by a rough Verdana 11px character width."""
    def width(text: str) -> int:
        return int(len(text) * 6.6) + 12
    lw, vw = width(label), width(value)
    total = lw + vw
    return f'''<svg xmlns="http://www.w3.org/2000/svg" width="{total}" height="20" role="img" aria-label="{label}: {value}">
<title>{label}: {value}</title>
<linearGradient id="s" x2="0" y2="100%"><stop offset="0" stop-color="#bbb" stop-opacity=".1"/><stop offset="1" stop-opacity=".1"/></linearGradient>
<clipPath id="r"><rect width="{total}" height="20" rx="3" fill="#fff"/></clipPath>
<g clip-path="url(#r)"><rect width="{lw}" height="20" fill="#555"/><rect x="{lw}" width="{vw}" height="20" fill="#007ec6"/><rect width="{total}" height="20" fill="url(#s)"/></g>
<g fill="#fff" text-anchor="middle" font-family="Verdana,Geneva,DejaVu Sans,sans-serif" font-size="11">
<text x="{lw / 2}" y="15" fill="#010101" fill-opacity=".3">{label}</text><text x="{lw / 2}" y="14">{label}</text>
<text x="{lw + vw / 2}" y="15" fill="#010101" fill-opacity=".3">{value}</text><text x="{lw + vw / 2}" y="14">{value}</text>
</g></svg>
'''


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("image")
    ap.add_argument("--previous")
    ap.add_argument("--summary")
    ap.add_argument("--badges")
    args = ap.parse_args()

    sizes = measure(args.image)
    prev = {}
    if args.previous and os.path.exists(args.previous):
        with open(args.previous) as f:
            prev = json.load(f)

    rows = [
        ("Binary (linux/amd64)", "binary_bytes"),
        ("Image, compressed (download)", "image_bytes"),
        ("Image, uncompressed", "image_uncompressed_bytes"),
    ]
    table = "| | Size | Change vs main |\n|---|---|---|\n" + "".join(
        f"| {name} | {mb(sizes[key])} | {change(sizes[key], prev.get(key))} |\n" for name, key in rows
    )
    print(table)
    if args.summary:
        with open(args.summary, "a") as f:
            f.write("### Sizes\n\n" + table + "\n")
    if args.badges:
        os.makedirs(args.badges, exist_ok=True)
        with open(os.path.join(args.badges, "binary.svg"), "w") as f:
            f.write(badge("binary", mb(sizes["binary_bytes"])))
        with open(os.path.join(args.badges, "image.svg"), "w") as f:
            f.write(badge("image", mb(sizes["image_bytes"])))
        with open(os.path.join(args.badges, "sizes.json"), "w") as f:
            json.dump(sizes, f, indent=2)


if __name__ == "__main__":
    main()
