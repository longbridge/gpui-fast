"""Builds SbixTest.ttf: one red square, U+E000, as an `sbix` PNG strike.

The font is made from nothing here, for gpui_apple's color bitmap font test
(`crates/gpui_apple/src/fast/color_bitmap_fonts.rs`), and is covered by this
repository's license. The output is byte-for-byte reproducible:

    uv run --with fonttools==4.60.1 python make_sbix_test_font.py SbixTest.ttf
"""

import struct
import sys
import zlib

from fontTools.fontBuilder import FontBuilder
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import newTable
from fontTools.ttLib.tables.sbixGlyph import Glyph
from fontTools.ttLib.tables.sbixStrike import Strike

UPEM = 1000
PPEM = 32


def png(size, rgba):
    """A `size` by `size` PNG of one opaque color."""

    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    row = b"\x00" + bytes(rgba) * size
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(row * size, 9))
        + chunk(b"IEND", b"")
    )


def scaffold(x0, y0, x1, y1):
    # CoreText takes a glyph's bounds from `glyf` even when it draws the
    # `sbix` bitmap; a two-point contour has no area and so draws no ink.
    pen = TTGlyphPen(None)
    pen.moveTo((x0, y0))
    pen.lineTo((x1, y1))
    pen.closePath()
    return pen.glyph()


order = [".notdef", "square"]
builder = FontBuilder(UPEM, isTTF=True)
builder.setupGlyphOrder(order)
builder.setupCharacterMap({0xE000: "square"})
builder.setupGlyf({".notdef": TTGlyphPen(None).glyph(), "square": scaffold(0, 0, UPEM, UPEM)})
builder.setupHorizontalMetrics({".notdef": (UPEM, 0), "square": (UPEM, 0)})
builder.setupHorizontalHeader(ascent=UPEM, descent=-200)
builder.setupNameTable({"familyName": "GPUI Sbix Test", "styleName": "Regular", "psName": "GPUISbixTest-Regular"})
builder.setupOS2(sTypoAscender=UPEM, sTypoDescender=-200, usWinAscent=UPEM, usWinDescent=200)
builder.setupPost()

sbix = newTable("sbix")
sbix.version = 1
sbix.flags = 1
strike = Strike(ppem=PPEM, resolution=72)
strike.glyphs = {
    ".notdef": Glyph(glyphName=".notdef"),
    "square": Glyph(glyphName="square", graphicType="png ", imageData=png(PPEM, (255, 0, 0, 255))),
}
sbix.strikes = {PPEM: strike}
builder.font["sbix"] = sbix
builder.font.recalcTimestamp = False
builder.font["head"].created = builder.font["head"].modified = 0
builder.save(sys.argv[1])
