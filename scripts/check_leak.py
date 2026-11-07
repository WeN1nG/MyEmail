"""发布前的信息泄露自检。

检查一个 `dist/<版本>/` 目录里的三个产物（exe / 源码 zip / 发布说明）是否夹带
本机标识、凭据形态或本机数据库里的真实内容。任何一项命中都应视为**发布阻塞**。

用法：
    python scripts/check_leak.py                      # 自动取 dist/ 下最新的版本目录
    python scripts/check_leak.py --dist dist/0.3.0    # 指定目录

退出码：0 全部通过；1 有阻塞项。
"""

import argparse
import io
import os
import re
import sqlite3
import sys
import zipfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def local_markers():
    """本机标识：全部**从环境推导**，不把用户名/路径写死在脚本里。

    这点很关键——脚本本身会被打进源码包，写死就等于在包里留了一份样本。
    只取「具体」标识（用户名、仓库绝对路径、用户目录），不取 `/home/`、`C:\\Users`
    这类通用前缀：它们本来就该出现在文档与跨平台代码里，当标记用只会永远误报。
    """
    markers = []
    user = os.environ.get('USERNAME') or os.environ.get('USER')
    if not user and os.environ.get('USERPROFILE'):
        user = os.path.basename(os.environ['USERPROFILE'])
    if user:
        markers.append(user.encode('utf-8', 'ignore'))

    repo = os.path.abspath(ROOT)
    markers.append(repo.encode('utf-8', 'ignore'))
    markers.append(repo.replace('/', '\\').encode('utf-8', 'ignore'))

    home = os.environ.get('USERPROFILE') or os.environ.get('HOME')
    if home:
        markers.append(home.encode('utf-8', 'ignore'))
        markers.append(home.replace('/', '\\').encode('utf-8', 'ignore'))

    # 去重 + 去掉空串与过短的碎片（2 字符以下当标记会误伤）。
    return [m for m in dict.fromkeys(markers) if len(m) >= 4]


MARKERS = local_markers()

# 凭据形态：出现即应处理。
#
# 每条都要带「误报过滤」——二进制里全是 OpenGL 着色器术语（`sk-borderspecularExponent`
# 之类恰好匹配 `sk-` 前缀），不排除就会永远误报。
SECRET_PATTERNS = [
    (rb'BEGIN [A-Z ]*PRIVATE KEY', '私钥'),
    (rb'\b[0-9a-f]{32}\b', '32 位 hex（和风 API Key 形态）'),
    (rb'(?i)sk-[A-Za-z0-9]{20,}', 'OpenAI 风格 key'),
    (rb'(?i)xox[baprs]-[A-Za-z0-9-]{10,}', 'Slack token'),
    (rb'(?i)ghp_[A-Za-z0-9]{20,}', 'GitHub token'),
    (rb'(?i)AIza[0-9A-Za-z_\-]{35}', 'Google API key'),
]


def latest_dist():
    base = os.path.join(ROOT, 'dist')
    if not os.path.isdir(base):
        return None
    candidates = [
        os.path.join(base, name)
        for name in os.listdir(base)
        if os.path.isfile(os.path.join(base, name, 'myemail.exe'))
    ]
    if not candidates:
        return None
    return max(candidates, key=os.path.getmtime)


class Report:
    def __init__(self):
        self.bad = 0

    def fail(self, msg):
        print('  [阻塞] ' + msg)
        self.bad += 1

    def ok(self, msg):
        print('  OK：' + msg)


def check_exe(report, exe):
    print(f'=== {os.path.basename(os.path.dirname(exe))}/myemail.exe ===')
    if not os.path.exists(exe):
        report.fail('exe 不存在')
        return
    data = open(exe, 'rb').read()
    before = report.bad

    for marker in MARKERS:
        count = data.count(marker)
        if count:
            report.fail(f'含本机标识 {marker!r}：{count} 处')

    # 绝对路径（panic 位置信息）。发布构建应带 --remap-path-prefix，这里应为 0。
    #
    # 顶层目录**允许只有 1 个字符**（`C:\A\...` 这种短盘符下的一级目录很常见）：
    # 早先写成 `{2,}` 时正好把本机 `C:\A\CodingSpace\...` 的依赖源码路径漏掉了。
    paths = set()
    for match in re.finditer(rb'[A-Za-z]:\\[^\x00\x01-\x1f]{6,90}', data):
        path = match.group(0).decode('utf-8', 'ignore')
        if not re.match(r'^[A-Za-z]:\\[^\\/:*?"<>|]{1,}\\', path):
            continue
        # `C:\Windows\...` 是**每台 Windows 都一样的系统路径**，不携带本机身份。
        # 命中的是 fontdb 的静态字面量（`C:\Windows\Fonts` + `USERPROFILE` +
        # `AppData\Local\Microsoft\Windows\Fonts` 在二进制里相邻，被读成一整条），
        # 属于误报；真正的构建机路径不会落在这里。
        if path.lower().startswith('c:\\windows\\'):
            continue
        paths.add(path)
    if paths:
        report.fail(
            f'含绝对路径 {len(paths)} 条，示例：{sorted(paths)[0]}\n'
            '         → 用 bash scripts/build_release.sh 重建（带 --remap-path-prefix）'
        )

    for pattern, name in SECRET_PATTERNS:
        hits = set()
        for match in re.finditer(pattern, data):
            token = match.group(0)
            text = token.decode('latin1')
            # 真实 key 一定同时含大写、小写与数字；纯字母的 `sk-xxx` 是着色器术语。
            if name == 'OpenAI 风格 key':
                body = text[3:]
                if not (
                    re.search(r'[a-z]', body)
                    and re.search(r'[A-Z]', body)
                    and re.search(r'[0-9]', body)
                ):
                    continue
            hits.add(token[:40])
        if hits:
            report.fail(f'疑似{name}：{list(hits)[:2]}')

    # 与本机数据库里的真实内容比对（有数据库才查）。
    db = os.path.expandvars(r'%APPDATA%\MyEmail\myemail.db')
    if os.path.exists(db):
        con = sqlite3.connect(f'file:{db}?mode=ro', uri=True)
        cur = con.cursor()
        values = []
        for sql in (
            'SELECT email FROM accounts',
            'SELECT name FROM weather_locations',
            'SELECT subject FROM messages',
        ):
            try:
                cur.execute(sql)
                values += [row[0] for row in cur.fetchall() if row[0]]
            except Exception:
                pass
        con.close()
        leaked = [value for value in values if value.encode('utf-8', 'ignore') in data]
        if leaked:
            report.fail(f'含数据库里的真实内容 {len(leaked)} 项，示例：{leaked[0][:30]}')

    if report.bad == before:
        report.ok(f'无本机标识、无凭据、无绝对路径（{len(data) / 1024 / 1024:.1f} MB 已扫描）')


def check_zip(report, zip_path, version):
    print(f'=== {os.path.basename(zip_path)} ===')
    if not os.path.exists(zip_path):
        report.fail('源码 zip 不存在')
        return
    zf = zipfile.ZipFile(zip_path)
    if zf.testzip() is not None:
        report.fail('zip 损坏')
        return
    names = zf.namelist()
    before = report.bad

    data_ext = ('.db', '.db-wal', '.db-shm', '.sqlite', '.log', '.pem', '.key',
                '.p12', '.pfx', '.env', '.exe', '.dll', '.pdb')
    hits = [n for n in names if n.lower().endswith(data_ext)]
    if hits:
        report.fail(f'含数据/凭据/二进制文件：{hits[:5]}')

    for name in names:
        try:
            content = zf.read(name)
        except Exception:
            continue
        for marker in MARKERS:
            if marker in content:
                report.fail(f'{name} 含本机标识 {marker!r}')

    # 版本一致性：zip 里的 Cargo.toml 应与目录名一致（目录名像版本号时才查）。
    if re.match(r'^\d+\.\d+\.\d+', version or '') and 'Cargo.toml' in names:
        cargo = zf.read('Cargo.toml').decode('utf-8', 'replace')
        match = re.search(r'(?m)^version\s*=\s*"([^"]+)"', cargo)
        if match and match.group(1) != version:
            report.fail(f'zip 内版本 {match.group(1)} 与目录名 {version} 不一致（源码与二进制不同源？）')

    if report.bad == before:
        report.ok(f'{len(names)} 个条目，无数据文件、无本机标识、版本一致')


def check_notes(report, notes, version):
    print(f'=== {os.path.basename(notes)} ===')
    if not os.path.exists(notes):
        report.fail('发布说明不存在')
        return
    text = io.open(notes, encoding='utf-8').read()
    before = report.bad
    for marker in MARKERS + [b'@qq.com']:
        token = marker.decode('utf-8', 'ignore')
        if token and token in text:
            report.fail(f'含本机标识 {token!r}')
    if version and version not in text:
        report.fail(f'发布说明里没有出现版本号 {version}')
    if report.bad == before:
        report.ok('无本机标识，且写明了版本号')


def main():
    parser = argparse.ArgumentParser(description='发布前信息泄露自检')
    parser.add_argument('--dist', help='dist 下的版本目录，缺省取最新')
    args = parser.parse_args()

    dist = args.dist
    if dist and not os.path.isabs(dist):
        dist = os.path.join(ROOT, dist)
    if not dist:
        dist = latest_dist()
    if not dist or not os.path.isdir(dist):
        sys.exit('找不到发布目录：用 --dist 指定（例如 --dist dist/0.3.0）')

    version = os.path.basename(dist.rstrip('/\\'))
    print(f'发布前信息泄露自检\n目录：{os.path.relpath(dist, ROOT)}\n')

    report = Report()
    check_exe(report, os.path.join(dist, 'myemail.exe'))

    zips = [n for n in os.listdir(dist) if n.lower().endswith('.zip')]
    if not zips:
        report.fail('目录里没有源码 zip')
    else:
        check_zip(report, os.path.join(dist, zips[0]), version)

    check_notes(report, os.path.join(dist, 'RELEASE_NOTES.md'), version)

    print()
    if report.bad:
        print(f'=== 发现 {report.bad} 处问题，不应发布 ===')
        return 1
    print('=== 全部通过，可以发布 ===')
    return 0


if __name__ == '__main__':
    sys.exit(main())
