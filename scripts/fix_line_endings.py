"""把仓库内所有文本文件统一成 LF 行尾（.gitattributes 要求 `* text=auto eol=lf`）。

背景：仓库的 `.gitattributes` 是 `* text=auto eol=lf` 且 `core.safecrlf=true`，
工作区若混入 CRLF，`git add` 会直接报 `fatal: CRLF would be replaced by LF in <file>`。

**注意**：改文件必须用 `write_bytes()`——`write_text()` 在 Windows 上会用
默认换行转换重新写入 CRLF（这正是这个坑反复出现的原因）。

用法：
    python scripts/fix_line_endings.py           # 只检查并报告
    python scripts/fix_line_endings.py --fix     # 真的改写
"""

from __future__ import annotations

import pathlib
import subprocess
import sys

# 二进制扩展名：跳过。
BINARY_SUFFIXES = {
    ".ttf", ".otf", ".woff", ".woff2", ".mp3", ".png", ".jpg", ".jpeg",
    ".gif", ".ico", ".zip", ".exe", ".dll", ".pdb", ".pdf", ".gz",
}

# 这些目录是构建产物 / 依赖缓存，不属于源码。
SKIP_DIRS = {
    ".git", "target", "dist", ".workbuddy", ".review", "checktemp",
    ".spacetest", "node_modules", ".idea", ".vscode",
}


def tracked_files() -> list[pathlib.Path]:
    """列出 git 跟踪的文件（不含未跟踪的新文件）。"""
    out = subprocess.run(
        ["git", "ls-files"], capture_output=True, text=True, check=False
    ).stdout
    result = []
    for line in out.splitlines():
        line = line.strip()
        if not line:
            continue
        p = pathlib.Path(line)
        if p.suffix.lower() in BINARY_SUFFIXES:
            continue
        if any(part in SKIP_DIRS for part in p.parts):
            continue
        if p.exists() and p.is_file():
            result.append(p)
    return result


def has_crlf(data: bytes) -> bool:
    return b"\r\n" in data


def main() -> int:
    fix = "--fix" in sys.argv
    offenders: list[tuple[pathlib.Path, int]] = []

    for path in tracked_files():
        try:
            data = path.read_bytes()
        except OSError:
            continue
        if not has_crlf(data):
            continue
        count = data.count(b"\r\n")
        offenders.append((path, count))
        if fix:
            # **必须用 write_bytes** —— write_text 会重新写入 CRLF。
            path.write_bytes(data.replace(b"\r\n", b"\n"))

    if not offenders:
        print("全部文件已是 LF。")
        return 0

    for path, count in offenders:
        action = "已修正" if fix else "需要修正"
        print(f"{action}: {path}（{count} 处 CRLF）")
    print(f"\n共 {len(offenders)} 个文件" + ("已修正。" if fix else "，加 --fix 执行修正。"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
