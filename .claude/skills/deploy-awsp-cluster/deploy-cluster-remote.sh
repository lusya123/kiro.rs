#!/usr/bin/env bash
# 在服务器上执行:部署单个 aws-p 集群容器(host 网络,自动接入共享 Redis)。
# 用法: deploy-cluster-remote.sh <IMAGE> <N> [CONFIG_BASE]
set -uo pipefail
IMAGE="$1"; N="$2"; BASE="${3:-/home/ubuntu}"
CFG="$BASE/kiro.rs-$N/config"

if [ ! -f "$CFG/config.json" ]; then
  echo "  ❌ 配置不存在: $CFG/config.json —— 请先准备好该容器的 config"; exit 3
fi
[ -f "$CFG/credentials.json" ] || echo "  ⚠️ 缺少 credentials.json,容器可能连不上上游"

# 安全:重建前备份一次 config(含 credentials.json)
cp -rn "$CFG" "$BASE/kiro.rs-$N/config.bak.cluster" 2>/dev/null || true

docker stop "kiro-rs-$N" >/dev/null 2>&1 || true
docker rm   "kiro-rs-$N" >/dev/null 2>&1 || true
# host 网络:与其他 aws-p 容器共享主机 loopback 上的唯一 Redis(127.0.0.1:46379)。
# 应用启动时自动:检测 46379 → 有则连(client)、无则起内嵌服务(owner)。无需额外配置。
docker run -d --name "kiro-rs-$N" --restart unless-stopped \
  --network host \
  -v "$CFG:/app/config" \
  "$IMAGE" >/dev/null
sleep 4

PORT=$(python3 -c "import json;print(json.load(open('$CFG/config.json'))['port'])" 2>/dev/null)
KEY=$(python3 -c "import json;print(json.load(open('$CFG/config.json'))['apiKey'])" 2>/dev/null)

# **关键**:host 网络的端口 Docker 不会自动开防火墙(bridge+-p 才会),不放行的话
# sub2api 后端连不上 → 测试/调用报 HTTP 524 超时。这里自动放行 sub2api 后端 IP
# (仅后端可达、不对公网全开)。默认后端 43.156.228.59,可用 SUB2API_BACKEND 覆盖。
SUB2API_BACKEND="${SUB2API_BACKEND:-43.156.228.59}"
if sudo ufw allow from "$SUB2API_BACKEND" to any port "$PORT" proto tcp >/dev/null 2>&1; then
  FW="已放行←${SUB2API_BACKEND}"
else
  FW="⚠️放行失败(需手动 ufw allow from ${SUB2API_BACKEND} to any port ${PORT})"
fi

ROLE=$(docker logs "kiro-rs-$N" 2>&1 | grep -oE '角色=[a-z]+' | tail -1)
[ -z "$ROLE" ] && ROLE="角色=?"
HTTP=$(curl -s -o /dev/null -w '%{http_code}' -m 8 "http://localhost:${PORT}/v1/models" -H "x-api-key: $KEY" 2>/dev/null)
echo "  kiro-rs-$N: $(docker ps --format '{{.Status}}' -f name=kiro-rs-$N) | 集群${ROLE} | 端口${PORT} /v1/models=HTTP${HTTP} | 防火墙${FW} | creds=$(test -f $CFG/credentials.json && echo ok || echo MISSING)"
