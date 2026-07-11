#!/usr/bin/env bash
#
# deploy-awsp-cluster: 一条命令部署 aws-p 集群容器,自动接入共享 Redis。
#
# 效果:每个容器用 --network host 起,启动时自动检测主机 loopback 上的 Redis
#       (127.0.0.1:46379)—— 已有则连上(client)、没有则起内嵌服务(owner)。
#       第一个容器托管那唯一的 Redis,后面 N 个自动连上它,一整批像一个号池。
#       嵌入式 Redis 已打包在镜像里,无需单独的 Redis 容器、无需额外配置。
#
# 用法:
#   bash deploy-cluster.sh 51606                 # 部署单个,自动入群
#   bash deploy-cluster.sh 51606 51607 51608     # 一次部署多个
#   bash deploy-cluster.sh 51606 --sha 54b8ca    # 指定镜像 SHA
#   bash deploy-cluster.sh 51606 --image ghcr.io/lusya123/kiro-rs:aws-p-beta-54b8ca
# 默认镜像:自动沿用集群里现有 host 网络容器的镜像(保持一致);没有就用本地 HEAD 的 SHA。
set -uo pipefail

SERVER="ubuntu@43.156.115.199"
CONFIG_BASE="/home/ubuntu"
IMAGE=""; SHA=""; NUMBERS=()

usage() {
  cat <<'USAGE'
用法: deploy-cluster.sh <容器编号...> [选项]
  <容器编号>          一个或多个,如 51606 51607(对应 config: /home/ubuntu/kiro.rs-<N>/config)
  --sha <sha6>        用指定 SHA 的镜像(aws-p-beta-<sha6>)
  --image <ref>       用指定完整镜像引用
  --server <user@ip>  目标服务器(默认 ubuntu@43.156.115.199)
  --config-base <dir> config 根目录(默认 /home/ubuntu)
  -h | --help
凭据: 从 $SSHPASS 或同目录 .deploy-secret 读取 SSH 密码。
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --sha) SHA="$2"; shift 2;;
    --image) IMAGE="$2"; shift 2;;
    --server) SERVER="$2"; shift 2;;
    --config-base) CONFIG_BASE="$2"; shift 2;;
    -h|--help) usage; exit 0;;
    -*) echo "未知参数: $1"; usage; exit 2;;
    *) NUMBERS+=("$1"); shift;;
  esac
done
[ ${#NUMBERS[@]} -eq 0 ] && { echo "❌ 至少给一个容器编号,如: deploy-cluster.sh 51606"; usage; exit 2; }

SKILL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [ -z "${SSHPASS:-}" ]; then
  if [ -f "$SKILL_DIR/.deploy-secret" ]; then SSHPASS="$(tr -d '\r\n' < "$SKILL_DIR/.deploy-secret")";
  else echo "❌ 缺少 SSH 密码: 设置 \$SSHPASS 或创建 $SKILL_DIR/.deploy-secret"; exit 1; fi
fi
export SSHPASS
command -v sshpass >/dev/null || { echo "❌ 需要 sshpass"; exit 1; }
SSH="sshpass -e ssh -o StrictHostKeyChecking=no -o ConnectTimeout=90"
SCP="sshpass -e scp -o StrictHostKeyChecking=no"

# ---- 决定镜像 ----
if [ -n "$SHA" ]; then IMAGE="ghcr.io/lusya123/kiro-rs:aws-p-beta-${SHA}"; fi
if [ -z "$IMAGE" ]; then
  # 沿用集群里第一个 host 网络 kiro-rs 容器的镜像(让新容器与现有集群一致)
  IMAGE="$($SSH "$SERVER" 'for c in $(docker ps --format "{{.Names}}" -f name=kiro-rs-); do if [ "$(docker inspect "$c" --format "{{.HostConfig.NetworkMode}}")" = host ]; then docker inspect "$c" --format "{{.Config.Image}}"; break; fi; done' 2>/dev/null | tr -d "\r" | head -1)"
fi
if [ -z "$IMAGE" ]; then
  SHA6="$(git -C "$SKILL_DIR" rev-parse --short=6 HEAD 2>/dev/null || echo latest)"
  IMAGE="ghcr.io/lusya123/kiro-rs:aws-p-beta-${SHA6}"
  echo "ℹ️ 集群里暂无 host 网络容器,回退用本地 HEAD 镜像。"
fi
echo "使用镜像: $IMAGE"
echo "目标容器: ${NUMBERS[*]}"

# ---- 拉镜像(避开陈旧缓存)----
$SSH "$SERVER" "docker logout ghcr.io >/dev/null 2>&1 || true; docker pull '$IMAGE' 2>&1 | grep -E 'Status|Downloaded|Error' | tail -1"

# ---- 上传远端部署脚本 ----
$SCP "$SKILL_DIR/deploy-cluster-remote.sh" "$SERVER:/tmp/awsp_cluster_remote.sh" >/dev/null 2>&1

# ---- 逐个部署 ----
for N in "${NUMBERS[@]}"; do
  echo "════ 部署 kiro-rs-$N(host 网络,自动入群)════"
  $SSH "$SERVER" "bash /tmp/awsp_cluster_remote.sh '$IMAGE' '$N' '$CONFIG_BASE'"
done

echo ""
echo "✅ 完成。共享 Redis: 127.0.0.1:46379(全主机唯一,第一个容器托管、其余自动连上)。"
echo "   验证共享是否命中:对不同容器发同一带 cache_control 的请求,第二个应报 cache_read>0。"
