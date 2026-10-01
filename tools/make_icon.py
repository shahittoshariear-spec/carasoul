"""Rebuild assets/carasoul.ico from the icon export in assets/icons/.

The export ships PNGs at every size iOS and Android ask for, but Windows wants a
single multi-size .ico for the executable's own icon, and neither Rust nor a
`build.rs` can rasterise or repack one without pulling in an image crate on the
build side. So the .ico is generated once here and committed; `build.rs` only has
to hand it to `rc.exe`.

    python tools/make_icon.py

Needs Pillow (`pip install pillow`).
"""

from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
MASTER = ROOT / "assets" / "icons" / "ios" / "icon-1024.png"
OUT = ROOT / "assets" / "carasoul.ico"

# 16/20/24/32 cover the tray, taskbar and small Explorer views; 40/48 are the
# shell's medium views; 64/96/128/256 are the large ones (256 is what Explorer's
# "Extra large icons" and the Windows 11 Start menu use).
SIZES = [(16, 16), (20, 20), (24, 24), (32, 32), (40, 40), (48, 48), (64, 64), (96, 96), (128, 128), (256, 256)]


def main() -> None:
    master = Image.open(MASTER).convert("RGBA")
    master.save(OUT, format="ICO", sizes=SIZES)
    with Image.open(OUT) as ico:
        print(f"{OUT.relative_to(ROOT)}: {ico.size[0]}px, sizes {' '.join(str(s[0]) for s in ico.info['sizes'])}")


if __name__ == "__main__":
    main()
