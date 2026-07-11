#!/usr/bin/env bash
#
# deploy-quietfox: 把 aws-p 分支构建的镜像部署到 kiro-rs-quietfox 容器。
#
# 流程: (可选 push) -> 等 GitHub Actions 构建 aws-p-beta-<sha6> -> 按 SHA tag 拉镜像
#        (先 docker logout 避开 ghcr 过期凭据的陈旧缓存) -> 重建容器 -> 三方 digest 校验 -> 健康检查。
#
# 所有参数都可覆盖(新版本自动用当前 HEAD 的 SHA;端口/容器名/服务器都能改)。
set -uo pipefail

# ---- 默认值(按当前 quietfox 部署) ----
BRANCH="aws-p"
CONTAINER="kiro-rs-quietfox"
HOST_PORT="8990"          # 宿主机端口;容器内部固定监听 8990
INTERNAL_PORT="8990"
SERVER="ubuntu@43.156.115.199"
IMAGE_REPO="ghcr.io/lusya123/kiro-rs"
REPO_SLUG="lusya123/kiro.rs"                       # gh api 用
CONFIG_DIR="/home/ubuntu/kiro.rs-quietfox/config"  # 服务器上挂载到 /app/config 的配置目录
SHA=""                     # 空=用当前 HEAD 的短 SHA(6 位)
DO_PUSH=0
WAIT_BUILD=1
HEALTH_KEY=""              # 可选:提供 api-key 则用 /v1/models 做鉴权健康检查

usage() {
  cat <<'USAGE'
用法: deploy.sh [选项]
  --sha <sha6>        部署指定 SHA(默认: 当前 HEAD 前 6 位)
  --port <port>       宿主机端口(默认: 8990)
  --container <name>  容器名(默认: kiro-rs-quietfox)
  --server <user@ip>  目标服务器(默认: ubuntu@43.156.115.199)
  --branch <name>     分支(默认: aws-p)
  --image <repo>      镜像仓库(默认: ghcr.io/lusya123/kiro-rs)
  --config-dir <path> 服务器上配置目录(默认: /home/ubuntu/kiro.rs-quietfox/config)
  --repo <owner/repo> GitHub 仓库 slug,gh api 用(默认: lusya123/kiro.rs)
  --push              先 git push origin <branch> 再部署
  --no-build-wait     跳过等待 CI 构建(镜像已存在时用)
  --health-key <key>  用该 api-key 对 /v1/models 做鉴权健康检查
  -h | --help         显示帮助
凭据: 从环境变量 $SSHPASS 读取 SSH 密码;没有则读同目录 .deploy-secret 文件。
USAGE
}

while [ $# -gt 0 ]; do
  case "$1" in
    --sha) SHA="$2"; shift 2;;
    --port) HOST_PORT="$2"; shift 2;;
    --container) CONTAINER="$2"; shift 2;;
    --server) SERVER="$2"; shift 2;;
    --branch) BRANCH="$2"; shift 2;;
    --image) IMAGE_REPO="$2"; shift 2;;
    --config-dir) CONFIG_DIR="$2"; shift 2;;
    --repo) REPO_SLUG="$2"; shift 2;;
    --push) DO_PUSH=1; shift;;
    --no-build-wait) WAIT_BUILD=0; shift;;
    --health-key) HEALTH_KEY="$2"; shift 2;;
    -h|--help) usage; exit 0;;
    *) echo "未知参数: $1"; usage; exit 2;;
  esac
done

SKILL_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# ---- SSH 密码 ----
if [ -z "${SSHPASS:-}" ]; then
  if [ -f "$SKILL_DIR/.deploy-secret" ]; then
    SSHPASS="$(tr -d '\r\n' < "$SKILL_DIR/.deploy-secret")"
  else
    echo "❌ 缺少 SSH 密码: 请设置环境变量 SSHPASS 或创建 $SKILL_DIR/.deploy-secret"; exit 1
  fi
fi
export SSHPASS
SSH="sshpass -e ssh -o StrictHostKeyChecking=no -o ConnectTimeout=90"

command -v sshpass >/dev/null || { echo "❌ 需要 sshpass (brew install hudochenkov/sshpass/sshpass)"; exit 1; }

# ---- 定位仓库 & SHA ----
REPO_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)" || { echo "❌ 不在 git 仓库内"; exit 1; }
cd "$REPO_ROOT"
CUR_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
if [ "$CUR_BRANCH" != "$BRANCH" ]; then
  echo "⚠️  当前分支是 $CUR_BRANCH,目标分支是 $BRANCH(继续,但请确认)"
fi
FULL_SHA="$(git rev-parse HEAD)"
[ -z "$SHA" ] && SHA="$(git rev-parse --short=6 HEAD)"
TAG="${BRANCH}-beta-${SHA}"
IMAGE="${IMAGE_REPO}:${TAG}"

echo "════════════════════════════════════════════"
echo " 部署 deploy-quietfox"
echo "  分支     : $BRANCH  (当前 HEAD $CUR_BRANCH)"
echo "  SHA      : $SHA"
echo "  镜像     : $IMAGE"
echo "  容器     : $CONTAINER"
echo "  服务器   : $SERVER"
echo "  端口     : ${HOST_PORT}->${INTERNAL_PORT}"
echo "  配置目录 : $CONFIG_DIR"
echo "════════════════════════════════════════════"

# ---- 可选 push ----
if [ "$DO_PUSH" = 1 ]; then
  echo "▶ git push origin $BRANCH ..."
  git push origin "$BRANCH" 2>&1 | tail -1
fi

# ---- 等待 CI 构建 ----
if [ "$WAIT_BUILD" = 1 ]; then
  echo "▶ 等待 GitHub Actions 构建 $TAG ..."
  SHORT7="$(git rev-parse --short=7 HEAD)"
  for i in $(seq 1 60); do
    STATUS="$(gh api "repos/${REPO_SLUG}/actions/runs?branch=${BRANCH}&per_page=5" \
      --jq '[.workflow_runs[] | select(.head_sha|startswith("'"$SHORT7"'"))][0].status' 2>/dev/null)"
    [ "$STATUS" = "completed" ] && break
    sleep 20
  done
  CONC="$(gh api "repos/${REPO_SLUG}/actions/runs?branch=${BRANCH}&per_page=5" \
    --jq '[.workflow_runs[] | select(.head_sha|startswith("'"$SHORT7"'"))][0].conclusion' 2>/dev/null)"
  echo "  构建结果: ${CONC:-未找到}"
  if [ "$CONC" != "success" ]; then
    echo "❌ 构建未成功(${CONC:-未找到}),中止部署。"; exit 1
  fi
fi

# ---- ghcr 上该 tag 的 manifest digest ----
GH_TOKEN_REG="$(curl -s "https://ghcr.io/token?scope=repository:${IMAGE_REPO#ghcr.io/}:pull" \
  | python3 -c "import sys,json;print(json.load(sys.stdin).get('token',''))" 2>/dev/null)"
GHCR_DIGEST="$(curl -s -I -H "Authorization: Bearer $GH_TOKEN_REG" \
  -H "Accept: application/vnd.oci.image.index.v1+json" \
  "https://ghcr.io/v2/${IMAGE_REPO#ghcr.io/}/manifests/${TAG}" 2>/dev/null \
  | grep -i docker-content-digest | tr -d '\r' | awk '{print $2}')"
echo "▶ ghcr digest: ${GHCR_DIGEST:-未取到}"

# ---- 服务器上拉取 + 重建容器 ----
echo "▶ 在服务器上拉镜像并重建容器 ..."
$SSH "$SERVER" "
  set -e
  docker logout ghcr.io >/dev/null 2>&1 || true
  docker pull '$IMAGE' 2>&1 | grep -E 'Status|Downloaded|Error' | tail -1
  docker stop '$CONTAINER' >/dev/null 2>&1 || true
  docker rm '$CONTAINER' >/dev/null 2>&1 || true
  docker run -d --name '$CONTAINER' --restart unless-stopped \
    -p '${HOST_PORT}:${INTERNAL_PORT}' \
    -v '${CONFIG_DIR}:/app/config' \
    '$IMAGE' >/dev/null
  sleep 3
  echo \"  容器状态: \$(docker ps --format '{{.Status}}' -f name='$CONTAINER')\"
  echo \"SERVER_DIGEST=\$(docker image inspect \$(docker inspect '$CONTAINER' --format '{{.Image}}') --format '{{index .RepoDigests 0}}' 2>/dev/null | sed 's#.*@##')\"
" 2>&1 | tee /tmp/_deploy_quietfox_out.txt

SERVER_DIGEST="$(grep '^SERVER_DIGEST=' /tmp/_deploy_quietfox_out.txt | cut -d= -f2)"

# ---- 健康检查 ----
echo "▶ 健康检查(端口 $HOST_PORT) ..."
if [ -n "$HEALTH_KEY" ]; then
  CODE="$($SSH "$SERVER" "curl -s -o /dev/null -w '%{http_code}' -m 8 http://localhost:${HOST_PORT}/v1/models -H 'x-api-key: ${HEALTH_KEY}'" 2>/dev/null)"
else
  CODE="$($SSH "$SERVER" "curl -s -o /dev/null -w '%{http_code}' -m 8 http://localhost:${HOST_PORT}/v1/models" 2>/dev/null)"
fi
echo "  HTTP $CODE  ($([ "$CODE" = 200 ] || [ "$CODE" = 401 ] && echo 服务已起 || echo 疑似未就绪))"

# ---- 三方 digest 校验 ----
echo "════════════════════════════════════════════"
if [ -n "$GHCR_DIGEST" ] && [ "$GHCR_DIGEST" = "$SERVER_DIGEST" ]; then
  echo "✅ digest 一致: ghcr = 服务器 = ${SERVER_DIGEST:0:19}"
else
  echo "⚠️  digest 不一致  ghcr=${GHCR_DIGEST:0:19}  服务器=${SERVER_DIGEST:0:19}"
fi
echo "✅ 部署完成: $BRANCH @ $SHA -> 容器 $CONTAINER (端口 $HOST_PORT) @ $SERVER"
echo "════════════════════════════════════════════"
