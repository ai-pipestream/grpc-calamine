#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Convert a CSV to XLSX for the calamine-README reproduction.

calamine's published benchmark uses the NYC 311 1M-row sample, distributed
as CSV and converted to XLSX before measuring; the conversion method is not
stated, so this script is ours and is committed to keep the recipe
reproducible end to end. Cells are typed the way a spreadsheet import would:
int if it parses as int, else float if it parses as float, else text, empty
string as an empty cell. Note that this loses leading zeros on numeric-
looking codes, exactly as a spreadsheet import does.

One property of this converter matters to the measurements: openpyxl's
write-only mode streams rows and therefore writes every string INLINE, with
no sharedStrings table. Excel and LibreOffice both write an sst, so a file
from this script parses differently (no string dedup in the container, and
`use_string_table` has nothing to intern). For an sst-bearing conversion,
use LibreOffice instead and measure both:

    soffice --headless --convert-to xlsx --outdir out input.csv

Usage:
    python3 csv_to_xlsx.py input.csv output.xlsx sheet-title

Needs openpyxl (the demos/python-client venv has it).
"""

import csv
import sys

from openpyxl import Workbook


def typed(v: str):
    if v == "":
        return None
    try:
        return int(v)
    except ValueError:
        pass
    try:
        return float(v)
    except ValueError:
        return v


def main() -> None:
    src, dst, title = sys.argv[1], sys.argv[2], sys.argv[3]
    wb = Workbook(write_only=True)
    ws = wb.create_sheet(title=title)
    with open(src, newline="", encoding="utf-8") as f:
        for n, row in enumerate(csv.reader(f)):
            ws.append([typed(v) for v in row])
            if n % 100_000 == 0:
                print(f"{n} rows", flush=True)
    wb.save(dst)
    print("saved", dst)


if __name__ == "__main__":
    main()
