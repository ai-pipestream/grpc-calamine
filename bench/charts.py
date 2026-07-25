#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Render the charts in charts/ from the captured numbers in RESULTS.md.

The values below are transcribed from RESULTS.md (captured 2026-07-24/25 on
a Ryzen 9 9950X3D, second machine a Ryzen 9 9950X over 10 GbE; every arm
digest-checked). Regenerate after replacing the captures:

    python3 charts.py

Plain SVG, no dependencies, neutral colors that read on light and dark
backgrounds.
"""

import os

# (label, seconds) per dataset. "gRPC" arms are grpc-calamine.
WALL = {
    "105.7 MB workbook, 985k rows, 7.9M cells": [
        ("calamine, in process (Rust)", 2.10),
        ("gRPC stream, loopback (Rust client)", 1.86),
        ("python-calamine, in process", 5.56),
        ("gRPC stream into Python", 4.80),
        ("openpyxl read_only", 20.6),
    ],
    "NYC 311 sample, 1M rows, 41M cells (186 MB)": [
        ("calamine, in process (Rust)", 6.02),
        ("gRPC stream, loopback (Rust client)", 6.60),
        ("python-calamine, in process", 17.8),
        ("gRPC stream into Python", 18.3),
        ("openpyxl read_only", 55.8),
    ],
}

# (mode, MB on the socket, multiple of the source file) per dataset.
WIRE = {
    "105.7 MB workbook": [
        ("plain", 789, 7.46),
        ("zstd", 290, 2.74),
        ("use_string_table", 173, 1.63),
        ("both", 62, 0.58),
    ],
    "NYC 311 (186 MB)": [
        ("plain", 662, 3.56),
        ("zstd", 158, 0.85),
        ("use_string_table", 301, 1.62),
        ("both", 104, 0.56),
    ],
}

# Streaming the 105.7 MB workbook from the second machine over a real link:
# (link, [(mode, seconds)...]). Filled from the network captures.
NETWORK = {
    "10 GbE LAN": [
        ("plain", 2.01),
        ("use_string_table", 2.06),
        ("both", 2.20),
    ],
    "shaped 1 Gbit/s": [
        ("plain", 8.64),
        ("use_string_table", 2.33),
        ("both", 1.83),
    ],
    "shaped 250 Mbit/s": [
        ("plain", 26.3),
        ("use_string_table", 5.64),
        ("both", 2.53),
    ],
}

MODE_COLORS = {
    "plain": "#8b96a8",
    "zstd": "#e0973c",
    "use_string_table": "#4478d0",
    "both": "#2fa197",
    "calamine, in process (Rust)": "#8b96a8",
    "gRPC stream, loopback (Rust client)": "#4478d0",
    "python-calamine, in process": "#b0a0d8",
    "gRPC stream into Python": "#4478d0",
    "openpyxl read_only": "#c98a9a",
}
TEXT = "#7d8590"
STRONG = "#98a1ab"

FONT = 'font-family="ui-monospace,SFMono-Regular,Menlo,monospace"'


def bar_chart(title, panels, value_label, out, shared_scale=None, marker=None):
    """One SVG of horizontal-bar panels: [(panel_title, [(label, value, color, text)])]."""
    width, label_w, value_w = 780, 272, 120
    bar_area = width - label_w - value_w - 20
    bar_h, gap, panel_head, panel_gap = 20, 7, 34, 18
    body = []
    y = 34
    for panel_title, rows in panels:
        scale_max = shared_scale or max(v for _, v, _, _ in rows) * 1.02
        body.append(
            f'<text x="0" y="{y}" fill="{STRONG}" font-size="13" font-weight="600" {FONT}>{panel_title}</text>'
        )
        y += panel_head - 20
        top = y
        for label, value, color, text in rows:
            w = max(2, value / scale_max * bar_area)
            body.append(
                f'<text x="{label_w - 8}" y="{y + bar_h - 6}" fill="{TEXT}" font-size="12" text-anchor="end" {FONT}>{label}</text>'
            )
            body.append(
                f'<rect x="{label_w}" y="{y}" width="{w:.1f}" height="{bar_h}" rx="2" fill="{color}"/>'
            )
            body.append(
                f'<text x="{label_w + w + 8:.1f}" y="{y + bar_h - 6}" fill="{TEXT}" font-size="12" {FONT}>{text}</text>'
            )
            y += bar_h + gap
        if marker is not None:
            mx = label_w + marker / scale_max * bar_area
            body.append(
                f'<line x1="{mx:.1f}" y1="{top - 4}" x2="{mx:.1f}" y2="{y - 3}" stroke="{TEXT}" stroke-dasharray="4 3" stroke-width="1"/>'
            )
            body.append(
                f'<text x="{mx + 6:.1f}" y="{top + 8}" fill="{TEXT}" font-size="11" {FONT}>size of the source file</text>'
            )
        y += panel_gap
    height = y + 6
    svg = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-label="{title}">',
        f'<text x="0" y="16" fill="{STRONG}" font-size="14" font-weight="600" {FONT}>{title}</text>',
        f'<text x="{width}" y="16" fill="{TEXT}" font-size="11" text-anchor="end" {FONT}>{value_label}</text>',
        *body,
        "</svg>",
    ]
    os.makedirs("charts", exist_ok=True)
    with open(out, "w") as f:
        f.write("\n".join(svg) + "\n")
    print("wrote", out)


def main():
    bar_chart(
        "Wall clock to read every cell, digest-checked (2026-07-24)",
        [
            (
                panel,
                [
                    (label, v, MODE_COLORS[label], f"{v:.2f} s" if v < 10 else f"{v:.1f} s")
                    for label, v in rows
                ],
            )
            for panel, rows in WALL.items()
        ],
        "seconds, lower is better; panel scales differ",
        "charts/wall-clock.svg",
    )

    bar_chart(
        "Bytes on the socket for the full row stream (2026-07-24)",
        [
            (
                panel,
                [
                    (mode, mult, MODE_COLORS[mode], f"{mb} MB ({mult:g}x)")
                    for mode, mb, mult in rows
                ],
            )
            for panel, rows in WIRE.items()
        ],
        "multiple of the source file, lower is better",
        "charts/wire-bytes.svg",
        shared_scale=7.8,
        marker=1.0,
    )

    if any(NETWORK.values()):
        bar_chart(
            "Streaming the 105.7 MB workbook from another machine (2026-07-25)",
            [
                (
                    link,
                    [
                        (mode, v, MODE_COLORS[mode], f"{v:.2f} s" if v < 10 else f"{v:.1f} s")
                        for mode, v in rows
                    ],
                )
                for link, rows in NETWORK.items()
                if rows
            ],
            "seconds for all 985k rows, lower is better; shared scale",
            "charts/network.svg",
            shared_scale=max(v for rows in NETWORK.values() for _, v in rows) * 1.02,
        )


if __name__ == "__main__":
    main()
