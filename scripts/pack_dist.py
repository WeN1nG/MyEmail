"""打发布包：exe + 同源源码 zip + 发布说明 → dist/<版本>/。

设计要点
- **源码清单用 git**：`git ls-files -z --cached --others --exclude-standard`
  —— 既包含已提交文件，也包含尚未提交的新文件，但不含 `target/`、`dist/`、
  `.workbuddy/`、`.review/` 这些被忽略的目录。绕开中文路径的 C 转义问题用 NUL 分隔。
- **exe 必须来自带路径重映射的构建**：见 `scripts/build_release.sh`。
  本脚本只做复制，不负责构建（分开才能看清是哪一步出的问题）。
- **发布说明从 `doc/<版本>发布说明.md` 复制**：`dist/` 被 .gitignore 排除，
  说明写在 dist 里等于没存档；放在 doc/ 才进版本库。
- zip 用 Python `zipfile` 写：`/` 分隔 + 非 ASCII 自动带 UTF-8 标志 0x800，
  Windows 上也没有可靠的 `zip` 命令。

用法：
    python scripts/pack_dist.py --version 0.3.0
    python scripts/pack_dist.py --version 0.3.0 --skip-build-check   # 明确知道 exe 是新的
"""

import argparse
import os
import shutil
import subprocess
import sys
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# 即使被 git 跟踪也不该进源码包的东西（防御性；正常情况下 .gitignore 已挡住）。
EXCLUDE_PREFIXES = ('target/', 'dist/', '.workbuddy/', '.review/', '.git/')
EXCLUDE_SUFFIXES = ('.exe', '.pdb', '.dll')


def tracked_files():
    """git 跟踪 + 未忽略的新文件；NUL 分隔避免中文路径转义。"""
    out = subprocess.run(
        ['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'],
        cwd=ROOT,
        stdout=subprocess.PIPE,
        check=True,
    ).stdout
    files = []
    for rel in out.decode('utf-8').split('\0'):
        if not rel:
            continue
        if rel.startswith(EXCLUDE_PREFIXES) or rel.lower().endswith(EXCLUDE_SUFFIXES):
            continue
        if not os.path.exists(os.path.join(ROOT, rel)):
            continue  # 已删除但仍在索引里的条目
        files.append(rel)
    return sorted(set(files))


# 影响二进制的路径前缀：只有这些文件比 exe 新，才算「源码与二进制不同源」。
# 文档改动不需要重建 exe，否则每次改 README 都得等 8 分钟。
CODE_PREFIXES = ('crates/', 'assets/')
CODE_FILES = ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml', 'deny.toml')


def newest_source_mtime(files):
    newest, newest_file = 0.0, ''
    for rel in files:
        if not (rel.startswith(CODE_PREFIXES) or rel in CODE_FILES):
            continue
        mtime = os.path.getmtime(os.path.join(ROOT, rel))
        if mtime > newest:
            newest, newest_file = mtime, rel
    return newest, newest_file


def main():
    parser = argparse.ArgumentParser(description='打发布包')
    parser.add_argument('--version', required=True, help='版本号，同时用作 dist 子目录名，例如 0.3.0')
    parser.add_argument('--skip-build-check', action='store_true', help='跳过「exe 是否比源码旧」的检查')
    args = parser.parse_args()

    exe_src = os.path.join(ROOT, 'target', 'release', 'myemail.exe')
    dist = os.path.join(ROOT, 'dist', args.version)
    zip_path = os.path.join(dist, f'{args.version}code.zip')
    notes_src = os.path.join(ROOT, 'doc', f'{args.version}发布说明.md')

    if not os.path.exists(exe_src):
        sys.exit(f'找不到 release 二进制：{exe_src}\n先跑 bash scripts/build_release.sh')

    files = tracked_files()
    if not files:
        sys.exit('git 里没有任何可打包文件')

    newest_mtime, newest_file = newest_source_mtime(files)
    if not args.skip_build_check and os.path.getmtime(exe_src) < newest_mtime:
        sys.exit(
            'exe 比源码旧，源码与二进制会不同源：\n'
            f'  exe      : {exe_src}\n'
            f'  较新的源码: {newest_file}\n'
            '先重新构建（bash scripts/build_release.sh），或确认无误后加 --skip-build-check'
        )

    os.makedirs(dist, exist_ok=True)

    if os.path.exists(zip_path):
        os.remove(zip_path)
    with zipfile.ZipFile(zip_path, 'w', zipfile.ZIP_DEFLATED, compresslevel=9) as zf:
        for rel in files:
            zf.write(os.path.join(ROOT, rel), rel)

    exe_dst = os.path.join(dist, 'myemail.exe')
    shutil.copy2(exe_src, exe_dst)

    notes_dst = os.path.join(dist, 'RELEASE_NOTES.md')
    if os.path.exists(notes_src):
        shutil.copy2(notes_src, notes_dst)
    else:
        print(f'[警告] 缺少发布说明 {notes_src}，dist 里不会有 RELEASE_NOTES.md')

    def mb(path):
        return os.path.getsize(path) / 1024 / 1024

    print(f'版本    : {args.version}')
    print(f'产物目录: {os.path.relpath(dist, ROOT)}')
    print(f'源码条目: {len(files)}')
    print(f'zip     : {mb(zip_path):.2f} MB')
    print(f'exe     : {mb(exe_dst):.2f} MB')
    if os.path.exists(notes_dst):
        print(f'说明    : {os.path.relpath(notes_dst, ROOT)}')
    print('\n下一步：python scripts/check_leak.py')


if __name__ == '__main__':
    main()
