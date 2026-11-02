"""Regenerate the website's images from the app's screenshots and icons.

Needs Python 3 with Pillow. From the repository's root:

    py site/make_images.py

It reads docs/screenshots and crates/ferrisping/assets and writes into site/public.
"""

from pathlib import Path

from PIL import Image

SITE = Path(__file__).resolve().parent
REPO = SITE.parent
SHOTS = REPO / "docs" / "screenshots"
ASSETS = REPO / "crates" / "ferrisping" / "assets"
PUBLIC = SITE / "public"
IMG = PUBLIC / "img"

# Published name -> screenshot. Only the synthetic renders from the render_screenshots test belong here.
SCREENSHOTS = {
    "lanes-dark": "ferrisping-dark.png",
    "lanes-light": "ferrisping-light.png",
    "route-dark": "ferrisping-route.png",
    "route-light": "ferrisping-route-light.png",
}
# Each screenshot is published at full size and at two-thirds, for phones and 1x screens.
# index.html names these widths in its srcset attributes.
WIDTHS = (1770, 1180)
WEBP_QUALITY = 90

# The light page and the app's light surface, behind the opaque home-screen icon.
TOUCH_ICON_BACKGROUND = (252, 252, 251)


def resize(image, width):
    if image.width == width:
        return image
    return image.resize((width, round(image.height * width / image.width)), Image.LANCZOS)


def screenshots():
    for name, source in SCREENSHOTS.items():
        shot = Image.open(SHOTS / source).convert("RGB")
        if shot.width != WIDTHS[0]:
            raise SystemExit(f"{source} is {shot.width} px wide; update WIDTHS here and the srcsets in index.html")
        for width in WIDTHS:
            resize(shot, width).save(IMG / f"{name}-{width}.webp", quality=WEBP_QUALITY, method=6)

    # The link preview for chat apps and social sites: the top of the dark lanes view at 1200x630.
    dark = resize(Image.open(SHOTS / SCREENSHOTS["lanes-dark"]).convert("RGB"), 1200)
    dark.crop((0, 0, 1200, 630)).save(IMG / "og.png", optimize=True)


def icons():
    # The SVGs as drawn for the app: the full crab for the page, the simplified one for browser tabs.
    (PUBLIC / "ferrisping.svg").write_bytes((ASSETS / "ferrisping.svg").read_bytes())
    (PUBLIC / "favicon.svg").write_bytes((ASSETS / "ferrisping-small.svg").read_bytes())

    # favicon.ico keeps the app's own small frames, which use the simplified crab.
    ico = Image.open(ASSETS / "ferrisping.ico")
    frames = [ico.ico.getimage((size, size)) for size in (16, 32, 48)]
    frames[-1].save(PUBLIC / "favicon.ico", format="ICO", sizes=[f.size for f in frames], append_images=frames[:-1])

    # iOS shows home-screen icons on black where they're transparent, so this one gets a background.
    crab = Image.open(ASSETS / "ferrisping-256.png").convert("RGBA").resize((148, 148), Image.LANCZOS)
    touch = Image.new("RGBA", (180, 180), TOUCH_ICON_BACKGROUND + (255,))
    touch.alpha_composite(crab, (16, 16))
    touch.convert("RGB").save(PUBLIC / "apple-touch-icon.png", optimize=True)


def main():
    IMG.mkdir(parents=True, exist_ok=True)
    screenshots()
    icons()
    for path in sorted(PUBLIC.rglob("*")):
        if path.suffix in {".webp", ".png", ".ico", ".svg"}:
            print(f"{path.relative_to(SITE)}  {path.stat().st_size // 1024} KB")


if __name__ == "__main__":
    main()
