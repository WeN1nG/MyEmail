"""生成内嵌 CJK 字体子集：Noto Sans SC 静态 400 + GB2312 常用字。

输入：C:/Windows/Fonts/NotoSansSC-VF.ttf（Noto 字体，OFL 1.1）
输出：assets/fonts/cjk-subset.ttf（OFL.txt 为固定许可证原文，不覆盖）
"""

from pathlib import Path

from fontTools.subset import Options, Subsetter
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont

ROOT = Path(__file__).resolve().parents[1]
SOURCE = Path(r"C:\Windows\Fonts\NotoSansSC-VF.ttf")
STATIC = Path(__file__).with_name("NotoSansSC-Regular.ttf")
OUT = ROOT / "assets" / "fonts" / "cjk-subset.ttf"


def collect_chars() -> set[str]:
    chars: set[str] = set()
    # UI 文案与代码里出现的所有字符。
    for path in (ROOT / "crates").rglob("*.rs"):
        chars.update(path.read_text(encoding="utf-8"))
    # ASCII 可打印字符。
    chars.update(chr(code) for code in range(0x20, 0x7F))
    # Latin-1 补充（欧洲姓名、重音符号）。
    chars.update(chr(code) for code in range(0xA0, 0x100))
    # 希腊字母与西里尔字母。
    chars.update(chr(code) for code in range(0x370, 0x400))
    chars.update(chr(code) for code in range(0x400, 0x500))
    # CJK 标点、全角字符。
    chars.update(chr(code) for code in range(0x3000, 0x3040))
    chars.update(chr(code) for code in range(0xFF00, 0xFFF0))
    # GB2312 一、二级汉字（覆盖绝大多数简体中文邮件）。
    for high in range(0xB0, 0xF8):
        for low in range(0xA1, 0xFF):
            try:
                chars.add(bytes([high, low]).decode("gb2312"))
            except UnicodeDecodeError:
                pass
    return chars


def main() -> None:
    if not SOURCE.is_file():
        raise SystemExit(f"缺少源字体：{SOURCE}")

    font = TTFont(SOURCE)
    instantiateVariableFont(font, {"wght": 400}, inplace=True, updateFontNames=True)
    font.save(STATIC)

    chars = collect_chars()
    codepoints = sorted({ord(ch) for ch in chars if ch})
    print(f"characters: {len(codepoints)}")

    subset_font = TTFont(STATIC)
    options = Options()
    options.layout_features = ["*"]
    options.name_IDs = ["*"]
    options.name_legacy = True
    options.notdef_outline = True
    options.recalc_bounds = True
    options.drop_tables = []
    subsetter = Subsetter(options=options)
    subsetter.populate(unicodes=codepoints)
    subsetter.subset(subset_font)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    subset_font.save(OUT)
    STATIC.unlink(missing_ok=True)
    print(f"wrote {OUT} ({OUT.stat().st_size / 1024 / 1024:.2f} MB)")


if __name__ == "__main__":
    main()