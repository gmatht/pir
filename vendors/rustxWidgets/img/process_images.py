from PIL import Image, ImageDraw, ImageFont
import re
from pathlib import Path

FONT_PATH = "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"
SRC = Path("in")
DST = Path("out")

DST.mkdir(exist_ok=True)

for src_path in sorted(SRC.glob("App*.png")):
    m = re.match(r"App(.+)\.png", src_path.name)
    if not m:
        continue
    label = m.group(1)
    dst_path = DST / f"{label}.png"

    img = Image.open(src_path).convert("RGBA")
    w, h = img.size

    font_size = int(min(w, h) * 0.25)
    font = ImageFont.truetype(FONT_PATH, font_size)

    bbox = font.getbbox(label)
    tw = bbox[2] - bbox[0]
    th = bbox[3] - bbox[1]

    pad_x = int(tw * 0.15)
    pad_y = int(th * 0.3)
    box_w = tw + 2 * pad_x
    box_h = th + 2 * pad_y
    bx = (w - box_w) // 2
    bg_cy = int(h * 0.7)
    tx_cy = int(h * 0.65)
    by = bg_cy - box_h // 2
    text_top = tx_cy - th // 2

    overlay = Image.new("RGBA", img.size, (0, 0, 0, 0))
    draw = ImageDraw.Draw(overlay)
    draw.rectangle([bx, by, bx + box_w, by + box_h], fill=(0, 0, 0, 128))
    draw.text((bx + pad_x, text_top), label, fill="white", font=font)

    result = Image.alpha_composite(img, overlay)
    result.save(dst_path)
    print(f"{src_path.name} -> {dst_path.name}")
