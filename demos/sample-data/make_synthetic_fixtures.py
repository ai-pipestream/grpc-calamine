#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Generate the synthetic fixtures whose <dimension> deliberately lies.

The fixtures from the calamine test suite all declare their extent honestly,
so nothing in the suite exercised the cases that have actually bitten this
server: a declared dimension larger than the content (trailing styled-blank
rows that calamine trims), and a declared dimension smaller than the content
(cells past the declaration that must not be dropped). ECMA-376 makes
`<dimension>` optional and writers get it wrong, so these are real inputs,
not adversarial ones.

Each workbook is the minimum OPC package calamine will open: one sheet,
numeric inline values, no shared strings, no styles part. Stored (not
deflated) so the bytes are inspectable with `unzip -p`.

Run from this directory: `python3 make_synthetic_fixtures.py`. The output is
committed, so this only needs re-running when a fixture changes.
"""

import zipfile

CONTENT_TYPES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
<Default Extension="xml" ContentType="application/xml"/>
<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
<Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>
"""

ROOT_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>
"""

WORKBOOK = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
<sheets><sheet name="Sheet1" sheetId="1" r:id="rId1"/></sheets>
</workbook>
"""

WORKBOOK_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
<Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>
"""

SHEET_TEMPLATE = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
<dimension ref="{dimension}"/>
<sheetData>
{sheet_data}</sheetData>
</worksheet>
"""

# Declares A1:C50 but content stops at row 4. Row 3 is an interior gap that
# must survive as an empty row; rows 10-11 are styled blanks (`<c>` with a
# style, no value) that the incremental reader yields as Empty cells and
# `Range::from_sparse` trims. This is the miniature of the 105 MB workbook
# whose declared 1,043,928 rows ended in 58,577 rows of styled blanks.
INFLATED = (
    "A1:C50",
    """<row r="1"><c r="A1"><v>1</v></c><c r="B1"><v>2</v></c><c r="C1"><v>3</v></c></row>
<row r="2"><c r="A2"><v>4</v></c></row>
<row r="4"><c r="B4"><v>5</v></c></row>
<row r="10"><c r="A10" s="1"/><c r="B10" s="1"/></row>
<row r="11"><c r="A11" s="1"/></row>
""",
)

# Declares A1:A1 but holds cells out to D5. Everything past the declaration
# must still stream; treating the declared extent as a filter is the bug that
# silently dropped 5 of temperature.xlsx's 6 cells.
UNDERDECLARED = (
    "A1:A1",
    """<row r="1"><c r="A1"><v>1</v></c></row>
<row r="5"><c r="D5"><v>9</v></c></row>
""",
)

# Declares C3:D4 but the content starts at A1, left of and above the declared
# start. `worksheet_range` rebuilds the extent from the cells it sees, so the
# A1 and B2 values are part of the sheet and must stream.
SHIFTED = (
    "C3:D4",
    """<row r="1"><c r="A1"><v>1</v></c></row>
<row r="2"><c r="B2"><v>2</v></c></row>
<row r="3"><c r="C3"><v>3</v></c><c r="D3"><v>4</v></c></row>
""",
)

# An honest declaration whose range simply does not start at column A. Pins
# the column-0 anchoring of streamed rows: the C3 value must sit at index 2
# of its row, behind two explicit empties.
OFFSET = (
    "C3:D4",
    """<row r="3"><c r="C3"><v>3</v></c><c r="D3"><v>4</v></c></row>
<row r="4"><c r="C4"><v>5</v></c><c r="D4"><v>6</v></c></row>
""",
)

# A declaration whose end is BEFORE its start. ECMA-376 does not forbid the
# ordering, and calamine computes the extent with unchecked u32 subtraction
# (xlsx/mod.rs:2789, and Dimensions::len at lib.rs:181), so a reversed range
# underflows: `total_cells` becomes astronomical in a release build, and in a
# build with overflow checks the parse panics outright. Both are the server's
# problem, because the client sees either a nonsense progress denominator or a
# stream that ends successfully having sent nothing at all.
REVERSED = (
    "C5:A1",
    """<row r="1"><c r="A1"><v>1</v></c></row>
<row r="5"><c r="C5"><v>9</v></c></row>
""",
)

# Rows in descending order. Nothing in ECMA-376 requires <row> elements to be
# sorted, and the `r` attribute on each <c> is what fixes the position, so
# calamine reads this correctly: `Range::from_sparse` sorts the cells it
# collected and reports A1=2, A2 empty, A3=1. The incremental densifier walks
# the cell stream in arrival order and only ever moves forward, so a row that
# arrives out of order is folded into the row already under construction.
OUT_OF_ORDER = (
    "A1:A3",
    """<row r="3"><c r="A3"><v>1</v></c></row>
<row r="1"><c r="A1"><v>2</v></c></row>
""",
)

# Fully reversed: 40 rows written last-to-first. Reachable one-pass because
# nothing is committed while the whole sheet still fits in the batcher's unsent
# queue, so every late row is placed rather than lost.
ROWS_DESCENDING = (
    "A1:A40",
    "".join(
        f'<row r="{r}"><c r="A{r}"><v>{r}</v></c></row>\n' for r in range(40, 0, -1)
    ),
)

# Out of order too late to repair: 600 ascending rows (which forces several
# batches onto the wire) and only then a new cell back in row 1. gRPC cannot
# retract a sent message, so the only honest outcome is a terminal in-band
# error. calamine's own buffered API reads this file fine; that gap is the
# documented cost of streaming in one pass.
ROWS_LATE_BACKWARDS = (
    "A1:B600",
    "".join(f'<row r="{r}"><c r="A{r}"><v>{r}</v></c></row>\n' for r in range(1, 601))
    + '<row r="1"><c r="B1"><v>999</v></c></row>\n',
)

# A declared width of 200,000 columns over a single cell at A1. calamine
# ignores the declaration and reports a 1x1 range. The declared end column is
# a file-controlled number used directly as an allocation length, and calamine
# only warns past its 16,384 column limit rather than clamping, so the real
# ceiling is the base-26 parse: `A1:ZZZZZZ1` is column 321,272,405, which is
# ~10 GB of CellData before a single cell is read. 200,000 is the largest
# value that demonstrates the same code path without making the test suite
# allocate anything alarming.
WIDE = (
    "A1:KIVI1",
    """<row r="1"><c r="A1"><v>1</v></c></row>
""",
)

# Two cells at opposite corners of Excel's grid, in a file of about 2 KB. The
# rows between them hold nothing, and there are 1,048,574 of them. Spelling
# each one out costs a message and a client-side object apiece: at the sheet's
# final width of 16,384 that is 17.2 billion cells, which is what OOM-killed a
# Node client. The whole run is one `row_gap` instead.
#
# calamine itself reports a 1,048,576 x 16,384 dense range for this file and
# has to allocate it; the streaming path never does, which is why this fixture
# is compared against calamine's *cells* rather than its rows.
CORNERS = (
    "A1:XFD1048576",
    """<row r="1"><c r="A1"><v>1</v></c></row>
<row r="1048576"><c r="XFD1048576"><v>2</v></c></row>
""",
)

# Interior gaps of every awkward shape in one sheet: a single blank row, a run
# of blanks, and a populated row immediately after another. Leading blanks
# (rows 1-2) and trailing blanks (rows 21-30, declared but never written) are
# trimmed rather than announced, so this pins both halves of the rule that a
# gap only ever covers the space *between* two populated runs.
GAPS = (
    "A1:A30",
    """<row r="3"><c r="A3"><v>3</v></c></row>
<row r="4"><c r="A4"><v>4</v></c></row>
<row r="6"><c r="A6"><v>6</v></c></row>
<row r="20"><c r="A20"><v>20</v></c></row>
""",
)


# Populated only from row 6 down, so a `HeaderRow::Row(2)` selection lands on a
# blank row. calamine inserts a synthetic empty cell at the chosen row before
# building the extent (xlsx/mod.rs:2702-2713), so its range then starts AT row 2
# and is 5 rows tall, rather than snapping to the first populated row:
#
#   FirstNonEmptyRow -> start=(5,0) end=(6,0) height=2
#   Row(2)           -> start=(2,0) end=(6,0) height=5
#
# The streaming path has to say the same thing, which it does by announcing
# rows 2-4 as a gap. This is the one case where a gap leads a stream.
BLANK_HEADER = (
    "A1:A10",
    """<row r="6"><c r="A6"><v>6</v></c></row>
<row r="7"><c r="A7"><v>7</v></c></row>
""",
)


def write(name: str, dimension: str, sheet_data: str) -> None:
    with zipfile.ZipFile(name, "w", zipfile.ZIP_STORED) as z:
        z.writestr("[Content_Types].xml", CONTENT_TYPES)
        z.writestr("_rels/.rels", ROOT_RELS)
        z.writestr("xl/workbook.xml", WORKBOOK)
        z.writestr("xl/_rels/workbook.xml.rels", WORKBOOK_RELS)
        z.writestr(
            "xl/worksheets/sheet1.xml",
            SHEET_TEMPLATE.format(dimension=dimension, sheet_data=sheet_data),
        )


if __name__ == "__main__":
    write("dimension_inflated.xlsx", *INFLATED)
    write("dimension_underdeclared.xlsx", *UNDERDECLARED)
    write("dimension_shifted.xlsx", *SHIFTED)
    write("dimension_offset.xlsx", *OFFSET)
    write("dimension_reversed.xlsx", *REVERSED)
    write("dimension_wide.xlsx", *WIDE)
    write("rows_out_of_order.xlsx", *OUT_OF_ORDER)
    write("rows_descending.xlsx", *ROWS_DESCENDING)
    write("rows_late_backwards.xlsx", *ROWS_LATE_BACKWARDS)
    write("corners.xlsx", *CORNERS)
    write("rows_with_gaps.xlsx", *GAPS)
    write("blank_header_row.xlsx", *BLANK_HEADER)
    print(
        "wrote dimension_inflated.xlsx dimension_underdeclared.xlsx "
        "dimension_shifted.xlsx dimension_offset.xlsx dimension_reversed.xlsx "
        "dimension_wide.xlsx rows_out_of_order.xlsx rows_descending.xlsx "
        "rows_late_backwards.xlsx corners.xlsx rows_with_gaps.xlsx "
        "blank_header_row.xlsx"
    )
