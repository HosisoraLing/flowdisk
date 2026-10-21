#!/bin/sh
set -eu
cd "$(dirname "$0")"
umask 077
if [ -e .env ]; then
    printf '%s\n' '保留已有 .env。'
else
    command -v openssl >/dev/null 2>&1 || { printf '%s\n' '需要 openssl 生成随机密钥。'; exit 1; }
    app_secret=$(openssl rand -hex 24)
    gateway_secret=$(openssl rand -hex 24)
    cat > .env <<CONFIG
APP_TOKEN=$app_secret
OPENLIST_PASSWORD=$gateway_secret
PUID=$(id -u)
PGID=$(id -g)
BIND_IP=127.0.0.1
APP_PORT=8080
OPENLIST_PORT=5244
CONNECTIONS_PER_FILE=8
ACTIVE_DOWNLOADS=2
GLOBAL_CONNECTIONS=24
COOKIE_SECURE=false
CONFIG
    printf '%s\n' '已生成 .env，权限 600。'
fi
mkdir -p data/openlist data/flowdisk downloads
printf '%s\n' '目录已准备。运行 docker compose up -d --build 启动。' \
  'FlowDisk: http://127.0.0.1:8080；网盘配置: http://127.0.0.1:5244' \
  '查看 .env 获取 APP_TOKEN 和 OPENLIST_PASSWORD。'
