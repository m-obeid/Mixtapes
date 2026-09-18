"""Print what Pillow and src/ui/cover_effects.py give for the synthetic covers in
rust/src/ui/cover_effects/pil.rs (fixtures). The Rust tests hold these values.

    python3 rust/tools/cover_effects_ref.py

With image paths as arguments, print the accent and the blurred mean luminance of
each instead, to hold against `cargo test -- --ignored compare_covers --nocapture`.
"""
import os
import sys
import warnings

warnings.simplefilter("ignore")
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "src"))

from PIL import Image, ImageEnhance, ImageFilter

from ui import color_utils
from ui import cover_effects as ce


def from_formula(w, h, pixel):
    data = bytearray()
    for y in range(h):
        for x in range(w):
            data += bytes(pixel(x, y))
    return Image.frombytes("RGB", (w, h), bytes(data))


def noisy(w, h):
    return from_formula(w, h, lambda x, y: ((x * 7 + y * 3) % 256, (x * x // 8 + y * 5) % 256, (x * y // 4) % 256))


def smooth(w, h):
    return from_formula(w, h, lambda x, y: (x * 255 // (w - 1), y * 255 // (h - 1), ((x // 16 + y // 16) % 2) * 200 + 20))


def gray_ramp(w, h):
    return from_formula(w, h, lambda x, y: (x * 255 // (w - 1),) * 3)


def flat(w, h):
    return Image.new("RGB", (w, h), (120, 120, 120))


def fnv(img):
    h = 0xCBF29CE484222325
    for b in img.tobytes():
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def show(name, img):
    print(f"{name} {img.size[0]}x{img.size[1]} fnv=0x{fnv(img):016x}")


def thumb(img):
    img = img.copy()
    img.thumbnail((128, 128), Image.LANCZOS)
    return img


def quantized(img):
    q = img.quantize(colors=32, method=Image.MEDIANCUT)
    pal = q.getpalette()
    return [(count, tuple(pal[i * 3:i * 3 + 3])) for count, i in q.getcolors()]


def blur_pipeline(img, dark):
    """The image steps of get_blurred_cover's worker."""
    w, h = img.size
    side = min(w, h)
    img = img.crop(((w - side) // 2, (h - side) // 2, (w + side) // 2, (h + side) // 2))
    img = img.resize((720, 720), Image.LANCZOS)
    img = img.filter(ImageFilter.GaussianBlur(radius=42))
    img = ImageEnhance.Color(img).enhance(1.25)
    img = ce._normalize_blur(img, dark)
    values = ce._blur_luminances(img)
    return img, (ce._percentile(values, 0.5), ce._percentile(values, 0.98 if dark else 0.02))


def synthetic():
    white = Image.new("RGB", (200, 150), (255, 255, 255))
    show("noisy_200x150_bicubic48", noisy(200, 150).resize((48, 48)))
    show("smooth_517x389_bicubic32", smooth(517, 389).resize((32, 32)))
    show("smooth_100x100_lanczos720", smooth(100, 100).resize((720, 720), Image.LANCZOS))
    show("noisy_517x389_thumb", thumb(noisy(517, 389)))
    show("smooth_800x800_thumb", thumb(smooth(800, 800)))
    show("noisy_1280x720_thumb", thumb(noisy(1280, 720)))
    show("smooth_97x131_thumb", thumb(smooth(97, 131)))
    show("smooth_100x60_thumb", thumb(smooth(100, 60)))
    show("smooth_300x300_blur42", smooth(300, 300).filter(ImageFilter.GaussianBlur(radius=42)))
    show("noisy_64x40_blur42", noisy(64, 40).filter(ImageFilter.GaussianBlur(radius=42)))
    show("noisy_200x150_blur5", noisy(200, 150).filter(ImageFilter.GaussianBlur(radius=5)))
    show("noisy_200x150_color125", ImageEnhance.Color(noisy(200, 150)).enhance(1.25))
    show("noisy_200x150_bright04", ImageEnhance.Brightness(noisy(200, 150)).enhance(0.4))
    show("noisy_200x150_bright17", ImageEnhance.Brightness(noisy(200, 150)).enhance(1.7))
    show("noisy_200x150_white03", Image.blend(noisy(200, 150), white, 0.3))
    for name, img in [("noisy_128x96", noisy(128, 96)), ("smooth_128x128", smooth(128, 128))]:
        print(f"quantize {name}: {quantized(img)}")
    dark_ramp = ImageEnhance.Brightness(gray_ramp(300, 200)).enhance(0.3)
    covers = [("noisy_517x389", noisy(517, 389)), ("smooth_800x800", smooth(800, 800)), ("smooth_97x131", smooth(97, 131)), ("gray_ramp_300x200", gray_ramp(300, 200)), ("gray_ramp_dark", dark_ramp), ("flat_300x300", flat(300, 300))]
    for name, img in covers:
        print(f"pick_accent {name}: {ce.pick_accent(img)!r} detail={ce.cover_detail(img)!r}")
    for name, img in [("smooth_517x389", smooth(517, 389)), ("noisy_200x150", noisy(200, 150))]:
        for dark in (True, False):
            out, backdrop = blur_pipeline(img, dark)
            scheme = "dark" if dark else "light"
            show(f"{name}_pipeline_{scheme}", out)
            print(f"backdrop {name} {scheme}: {backdrop!r}")


def real(paths):
    for path in paths:
        img = Image.open(path).convert("RGB")
        accent = ce.pick_accent(img)
        print(f"{path} accent={color_utils.to_css(accent) if accent else None} detail={ce.cover_detail(img):.6f}")
        if accent is None:
            continue
        for dark in (True, False):
            out, backdrop = blur_pipeline(img, dark)
            pixels = list(out.getdata())
            mean = sum(color_utils.relative_luminance(tuple(c / 255.0 for c in p)) for p in pixels) / len(pixels)
            scheme = "dark" if dark else "light"
            print(f"{path} {scheme} mean_luminance={mean:.6f} backdrop=({backdrop[0]:.6f}, {backdrop[1]:.6f})")


if __name__ == "__main__":
    real(sys.argv[1:]) if len(sys.argv) > 1 else synthetic()
