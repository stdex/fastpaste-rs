#!/usr/bin/env python3
"""Regenerate the packaged app icons from one source image.

    python3 packaging/make-icons.py [SOURCE]

Writes, relative to the repository root:

    packaging/linux/fastpaste-256.png   hicolor 256x256 (the .deb/.rpm)
    packaging/linux/fastpaste-32.png    hicolor 32x32
    packaging/windows/fastpaste.ico     16-256, for the exe and the installer

SOURCE defaults to the tray icon, crates/fastpaste-gui/ui/assets/tray.png.
That is a 32x32 two-colour glyph drawn on whole pixels, so whole-number
scales (32, 64, 128, 256) are nearest-neighbour and exactly as sharp as the
original; only 16, 24 and 48 are resampled. It is a placeholder: give real
artwork (a square PNG, 256x256 or larger) as SOURCE and every size is
resampled from it instead.

Needs Pillow.
"""

import sys
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
ICO_SIZES = [16, 24, 32, 48, 64, 128, 256]


def render(src: Image.Image, size: int) -> Image.Image:
    if size == src.width:
        return src.copy()
    if size > src.width and size % src.width == 0:
        return src.resize((size, size), Image.Resampling.NEAREST)
    return src.resize((size, size), Image.Resampling.LANCZOS)


def main() -> None:
    source = Path(sys.argv[1]) if len(sys.argv) > 1 else (
        ROOT / "crates/fastpaste-gui/ui/assets/tray.png"
    )
    src = Image.open(source).convert("RGBA")
    if src.width != src.height:
        sys.exit(f"{source}: not square ({src.width}x{src.height})")

    for size in (256, 32):
        out = ROOT / f"packaging/linux/fastpaste-{size}.png"
        render(src, size).save(out, optimize=True)
        print(out.relative_to(ROOT))

    frames = [render(src, s) for s in ICO_SIZES]
    ico = ROOT / "packaging/windows/fastpaste.ico"
    # The largest frame is the base; the others replace Pillow's own
    # downscales of it, which would blur the whole-pixel sizes.
    frames[-1].save(
        ico,
        format="ICO",
        sizes=[(s, s) for s in ICO_SIZES],
        append_images=frames[:-1],
    )
    print(ico.relative_to(ROOT))


if __name__ == "__main__":
    main()
