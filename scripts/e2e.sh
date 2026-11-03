#!/usr/bin/env bash
# 真实 IMAP/SMTP 端到端测试：启动 GreenMail 容器，跑 sync 集成测试。
#
# 需要 Docker Desktop 正在运行。首次执行会拉取镜像。
set -euo pipefail

cd "$(dirname "$0")/.."

compose_file="tests/e2e/docker-compose.yml"
compose=(docker compose -f "$compose_file")

cleanup() {
    "${compose[@]}" down -v >/dev/null 2>&1 || true
}
trap cleanup EXIT

"${compose[@]}" up -d

# 等待 IMAP 端口可用（Git Bash 自带 /dev/tcp）。
ready=0
for _ in $(seq 1 60); do
    if (exec 3<>/dev/tcp/127.0.0.1/3143) 2>/dev/null; then
        exec 3<&- 2>/dev/null || true
        ready=1
        break
    fi
    sleep 1
done
if [ "$ready" -ne 1 ]; then
    echo "GreenMail 未在 60 秒内就绪" >&2
    "${compose[@]}" logs >&2
    exit 1
fi

export MYEMAIL_E2E_IMAP_ADDR="${MYEMAIL_E2E_IMAP_ADDR:-127.0.0.1:3143}"
export MYEMAIL_E2E_SMTP_ADDR="${MYEMAIL_E2E_SMTP_ADDR:-127.0.0.1:3025}"
export MYEMAIL_E2E_USER="${MYEMAIL_E2E_USER:-alice@example.com}"
export MYEMAIL_E2E_PASSWORD="${MYEMAIL_E2E_PASSWORD:-secret}"

cargo test -p me-sync --test e2e_real_imap -- --ignored --nocapture