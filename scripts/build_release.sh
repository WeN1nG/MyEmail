#!/usr/bin/env bash
# release 构建（发布用）：路径重映射 + 崩溃重试。
#
# 为什么必须重映射：Rust 会把源码的**绝对路径**嵌进二进制（panic 位置信息）。
# 不处理的话发布出去的 exe 里能直接读到构建机的用户目录与仓库路径，
# `scripts/check_leak.py` 会把它判为发布阻塞。
#
# 重映射目标从环境推导，**不写死任何本机路径**（脚本会随源码包发布）：
#   - 仓库根目录   → REDACTED（项目自身的 panic 位置）
#   - 用户目录     → REDACTED（依赖源码在 ~/.cargo/registry 下，带用户名）
#
# 为什么带重试：本机 rustc 偶发 STATUS_ACCESS_VIOLATION（0xc0000005），崩在哪个 crate 不固定。
# cargo 会缓存已成功的 crate，直接重跑就能推进；连续失败 8 次才判定真失败。
#
# 注意：改 RUSTFLAGS 会让 cargo 认为所有依赖都要重编，第一次会慢很多（约 8 分钟），
# 之后 RUSTFLAGS 不变即可复用缓存。**不要在别处用不同的 RUSTFLAGS 构建 release**，否则缓存来回失效。
#
# 用法：bash scripts/build_release.sh [--jobs N]

set -u
cd "$(dirname "$0")/.."

# 转成 Windows 形式（rustc 记录的是 `C:\...`），没有 cygpath 就手工替换分隔符。
to_win_path() {
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -w "$1"
  else
    printf '%s' "$1" | sed 's|/|\\|g'
  fi
}

REPO_WIN="$(to_win_path "$(pwd)")"
USER_HOME="${USERPROFILE:-$HOME}"

export RUSTFLAGS=""
# 逐个加前缀，顺便跳过空值与重复项（同一个前缀加两遍没有意义）。
add_remap() {
  [ -n "${1:-}" ] || return 0
  case " $RUSTFLAGS " in
    *" --remap-path-prefix=$1=REDACTED "*) return 0 ;;
  esac
  RUSTFLAGS="$RUSTFLAGS --remap-path-prefix=$1=REDACTED"
}

add_remap "$REPO_WIN"
add_remap "$USER_HOME"

# **依赖源码**也在二进制里留 panic 位置：`CARGO_HOME` 与 rustup 工具链目录
# 完全可能不在用户目录下（本机就在 `C:\A\CodingSpace\Tools\Rust\...`）。
# 只重映射仓库与用户目录是不够的——`ttf-parser` 之类的 crate 会把
# `...\cargo\registry\src\<镜像>\<crate>-<ver>\src\lib.rs` 原样写进 exe。
add_remap "$(to_win_path "${CARGO_HOME:-$USER_HOME/.cargo}")"
SYSROOT="$(rustc --print sysroot 2>/dev/null)"
if [ -n "$SYSROOT" ]; then
  add_remap "$(to_win_path "$SYSROOT")"
  # rustup 的 toolchains / downloads 与 sysroot 同级，上推两级一并盖掉。
  add_remap "$(to_win_path "$(dirname "$(dirname "$SYSROOT")")")"
fi

export RUSTFLAGS

JOBS="-j 2"
if [ "${1:-}" = "--jobs" ] && [ -n "${2:-}" ]; then
  JOBS="-j $2"
fi

MAX=8
for i in $(seq 1 "$MAX"); do
  echo "=== 尝试 $i/$MAX ==="
  echo "RUSTFLAGS=$RUSTFLAGS"
  if cargo build --release -p me-app $JOBS 2>&1 | tail -20; then
    if [ -f target/release/myemail.exe ]; then
      echo "=== 构建成功 ==="
      ls -l target/release/myemail.exe
      exit 0
    fi
  fi
  echo "--- 本次未产出二进制，5 秒后重试 ---"
  sleep 5
done

echo "=== 连续 $MAX 次失败 ==="
exit 1
