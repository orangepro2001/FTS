"""生成 updater 的占位图标 src-tauri/icons/icon.ico(蓝底白色上升箭头)。

用法: python updater/tools/make_icon.py
"""
import pathlib
import struct

ROOT = pathlib.Path(__file__).resolve().parent.parent
OUT = ROOT / "src-tauri" / "icons" / "icon.ico"


def make_icon(path: pathlib.Path, sizes=(16, 32, 48, 256)) -> None:
    images = []
    for s in sizes:
        px = bytearray()
        c = (s - 1) / 2
        for y in range(s - 1, -1, -1):  # ICO 自底向上
            for x in range(s):
                edge = min(x, y, s - 1 - x, s - 1 - y)
                if edge < s * 0.10:
                    px += bytes((0x30, 0x6B, 0x1E, 255))  # 深蓝边框(BGR)
                    continue
                apy = y - s * 0.18
                apx = abs(x - c)
                in_arrow = 0 <= apy <= s * 0.55 and apx <= (s * 0.42 - apy) * 0.82 and y <= s * 0.78
                in_stem = abs(x - c) <= s * 0.07 and s * 0.55 <= y <= s * 0.80
                if in_arrow or in_stem:
                    px += bytes((255, 255, 255, 255))
                else:
                    px += bytes((0xD0, 0x8F, 0x25, 255))  # 品蓝(BGR)
        mask = bytearray((((s + 31) // 32) * 4) * s)
        # BMP 格式图标项 = BITMAPINFOHEADER(40B,biHeight=XOR+AND 两倍) + 像素 + AND 掩码
        bmp_header = struct.pack(
            "<IiiHHIIiiII", 40, s, s * 2, 1, 32, 0, len(px) + len(mask), 0, 0, 0, 0
        )
        images.append((s, bmp_header + bytes(px) + bytes(mask)))
    header = struct.pack("<HHH", 0, 1, len(images))
    offset = 6 + 16 * len(images)
    entries = b""
    data = b""
    for s, blob in images:
        entries += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(blob), offset)
        data += blob
        offset += len(blob)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(header + entries + data)
    print(f"icon written: {path} ({path.stat().st_size} bytes)")


if __name__ == "__main__":
    make_icon(OUT)
