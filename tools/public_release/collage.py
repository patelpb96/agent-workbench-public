"""Re-encode synthetic demo frames and make a labelled public README collage.

Requires Pillow (optional developer dependency). Redactions overwrite pixels with
solid RGB before any resampling. Final outputs still need local OCR + visual review.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path


def generate(source: Path, target: Path, redactions: dict | None = None):
    from PIL import Image, ImageDraw, ImageFont
    if target.exists():
        raise ValueError('Use a new media output directory')
    target.mkdir(parents=True, exist_ok=False)
    names = ('canvas', 'terminals', 'inspector-map', 'session-detail', 'model-picker', 'map-detail')
    images = {}
    for name in names:
        path = source / (name + '.png')
        with Image.open(path) as image:
            if image.width * image.height > 16_000_000:
                raise ValueError('Oversize frame')
            # Fresh RGB buffer; no EXIF/PNG text/ICC/alpha-layer metadata survives.
            clean = Image.new('RGB', image.size)
            clean.paste(image.convert('RGB'))
        draw = ImageDraw.Draw(clean)
        for rectangle in (redactions or {}).get(name, []):
            if len(rectangle) != 4 or any(type(n) is not int for n in rectangle):
                raise ValueError('Invalid solid-redaction rectangle')
            x0,y0,x1,y1 = rectangle
            if not 0 <= x0 < x1 <= clean.width or not 0 <= y0 < y1 <= clean.height:
                raise ValueError('Out-of-frame redaction')
            draw.rectangle((x0,y0,x1-1,y1-1),fill='#101820')
        clean.save(target/(name+'.png'),format='PNG')
        images[name] = clean
    canvas = Image.new('RGB',(1600,1130),'#0b1119')
    draw = ImageDraw.Draw(canvas)
    # A font is read locally for rasterization; no font file/name/path is exported.
    def font(size):
        for candidate in ('DejaVuSans.ttf','C:/Windows/Fonts/segoeui.ttf'):
            try: return ImageFont.truetype(candidate,size)
            except OSError: pass
        return ImageFont.load_default(size=size)
    draw.text((28,18),'Agent Workbench',font=font(38),fill='#edf4ff')
    draw.text((30,68),'Real UI. Synthetic sessions. No personal desktop pixels.',font=font(20),fill='#94acc8')
    def panel(name, label, x, y, w, h):
        draw.text((x,y),label,font=font(20),fill='#c5d8f4')
        image=images[name].copy()
        image.thumbnail((w,h-32),Image.Resampling.LANCZOS)
        px,py=x+(w-image.width)//2,y+32
        draw.rectangle((x-1,py-1,x+w,py+h-32),outline='#293c53',width=1)
        canvas.paste(image,(px,py))
    panel('canvas','Canvas / freeform agent windows',28,112,1030,678)
    panel('session-detail','Zoom / one session (crop)',1080,112,492,326)
    panel('model-picker','Zoom / model picker (crop)',1080,452,492,326)
    panel('map-detail','Inspector / tool-usage map (crop)',28,816,1030,252)
    panel('terminals','Terminals / split layout',1080,816,492,252)
    draw.text((28,1090),'Illustrative demo — in-page simulated backend; not a live desktop capture or execution evidence.',font=font(17),fill='#94acc8')
    canvas.save(target/'collage.png',format='PNG')


if __name__ == '__main__':
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--input',required=True);p.add_argument('--output',required=True);p.add_argument('--redactions')
    a=p.parse_args()
    generate(Path(a.input),Path(a.output),json.loads(Path(a.redactions).read_text(encoding='utf-8')) if a.redactions else None)
