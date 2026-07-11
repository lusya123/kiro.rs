---
name: deploy-awsp-cluster
description: One-command deploy of aws-p "cluster" containers on 43.156.115.199 that auto-share ONE Redis prompt-cache. Use when the user asks to deploy/add an aws-p container to the shared-cache cluster, spin up a kiro-rs-<N> that joins the shared Redis, scale the cluster, or fix cache split / a 524 error on aws-p containers. Every container MUST run --network host (bridge = each container isolates its own 127.0.0.1 loopback = cache split; never mix host and bridge). The first host container hosts the embedded Redis (127.0.0.1:46379), the rest auto-connect. host ports are NOT auto-firewalled by Docker, so the script auto-opens each port to the sub2api backend IP (else sub2api returns HTTP 524). For a never-split cluster, run a standalone redis container first (see SKILL body).
---

# deploy-awsp-cluster

一条命令把 aws-p 容器部署进**共享同一个 Redis** 的集群。

## 核心事实

- **自动 Redis 已在镜像里**,不用改镜像、不用单独的 Redis 容器。`src/cluster_cache.rs` 的嵌入式自举:启动时检测 `127.0.0.1:46379` → **有就连(client)、没有就起内嵌服务(owner)**。
- **必须全部 `--network host`,且绝不能和 bridge 混用**:只有 host 网络下,各容器的 `127.0.0.1` 才是同一个(全主机共享),自举才能碰头。**bridge 下 loopback 各自隔离 → 每个容器各起一个 Redis = 缓存分裂**。混用(部分 host 部分 bridge)= host 那批内部共享、bridge 那批各自独立,一样分裂。要一个大集群 → **8 个/N 个容器全 host,同一个镜像版本**。
- **⚠️ host 端口要手动开防火墙(否则 sub2api 报 524)**:host 网络的端口 **Docker 不会自动开防火墙**(bridge+-p 才会自动开)。不放行的话 sub2api 后端连不上 → 测试/调用报 **HTTP 524 超时**。本 skill 的部署脚本**已自动**给每个端口放行 sub2api 后端 IP(`ufw allow from 43.156.228.59 to any port <PORT>`,仅后端可达、不对公网开)。换后端 IP 用环境变量 `SUB2API_BACKEND` 覆盖。详见记忆 `awsp-hostnet-firewall-524`。
- 端口 **46379**(非生产 6379)、绑主机 loopback、不对外,**不影响生产 Redis**。
- 容器 config.json 需有 `host=0.0.0.0` + 各自 `port`;host 网络下应用直接绑主机端口,对外访问不变(不用 -p)。

## 缓存分裂 & "永不分裂"配置

- **嵌入式自举(默认,本脚本)**:第一个容器托管唯一 Redis。够用,但**那个 owner 容器一重启/重部,缓存清空,而且 client 不自动重连 → 会重新分裂**。适合"加新容器进已有集群"这种增量操作。
- **独立 Redis(最稳,永不分裂,推荐给"要一个大集群且长期不分裂")**:单独跑一个专用 `redis` 容器常驻(`--network host` 绑 `127.0.0.1:46379`、`--restart unless-stopped`),**先起它**,再让所有 app 容器 host-net 启动 → 它们都只当 client 连这个独立 Redis。任何 app 容器重启/重部都不影响缓存。代价:多一个常驻小容器。要走这条,先 `docker run -d --name awsp-redis --restart unless-stopped --network host redis:7-alpine redis-server --bind 127.0.0.1 --port 46379`,再照常用本脚本部署 app 容器(它们检测到 46379 已有就自动当 client)。
- **排查分裂**:`docker inspect kiro-rs-<N> --format '{{.HostConfig.NetworkMode}}'` 必须都是 `host`;有 `bridge` 的就是分裂源。谁真正托管 46379:`sudo ss -tlnp | grep 46379`。

## 怎么用

```bash
bash .claude/skills/deploy-awsp-cluster/deploy-cluster.sh 51606              # 部署一个,自动入群
bash .claude/skills/deploy-awsp-cluster/deploy-cluster.sh 51606 51607 51608  # 一次多个
bash .claude/skills/deploy-awsp-cluster/deploy-cluster.sh 51606 --sha 54b8ca # 指定镜像 SHA
```

- 每个编号 `<N>` 对应服务器上 `/home/ubuntu/kiro.rs-<N>/config`(需含 `config.json` + `credentials.json`,脚本会先备份一份 `config.bak.cluster`)。
- **镜像默认**:自动沿用集群里现有 host 网络容器的镜像(保持整批一致);集群为空时回退到本地 HEAD 的 `aws-p-beta-<sha6>`。用 `--sha`/`--image` 覆盖。
- 部署后脚本自报每个容器:运行状态、**集群角色(owner/client)**、端口、`/v1/models` HTTP 码、凭证是否在。

## 执行时要点(给助手)

1. 如实汇报每个容器的**角色**:第一个应是 `owner`(它把那唯一的 Redis 起起来),其余应是 `client`(连上已有的)。**若出现多个 owner = 缓存分裂**——多半是有容器还是 bridge 网络(用 `docker inspect ... NetworkMode` 查),把它们重部成 host 即可统一。
2. **重新部署"现任 owner"容器会清空共享缓存**(它托管的 Redis 随之重启)。只是给集群**加**新容器(如 51606)则无影响——新容器只连、不打断。若必须重部 owner,提醒用户缓存会短暂重建。想彻底避免这个,用上面的"独立 Redis"方案。
3. **部署后一定验证防火墙**:脚本会自动 `ufw allow from <后端> to any port <端口>`;完成后从 sub2api 后端(43.156.228.59)`curl http://43.156.115.199:<端口>/v1/models` 应返回 401(通),不是 8s 超时。否则 sub2api 测试会报 524。
4. 部署会重启/新建生产容器,属对外操作;除非已授权,先确认再跑。
5. 验证真共享:对**不同**容器发同一条带 `cache_control`、前缀 1024–4096 token 的请求,第二个容器应报 `cache_read>0`(跨容器命中);全新前缀应是 miss(创建)。
6. **opus-4-8 冷启动慢**:刚部署的容器第一次调 opus-4-8 可能 15-30s(IdC 账号冷启动),偶发 524;有流量后降到 ~3s。不是故障。sonnet/opus-4.7 不受影响。
7. 相关背景见记忆 `awsp-cluster-cache-hostnet`(host 网络前提)、`awsp-hostnet-firewall-524`(防火墙/524);单容器(非集群)部署见 `deploy-quietfox` skill。

## 凭据

SSH 密码从 `$SSHPASS` 或同目录 `.deploy-secret`(已 gitignore)读取。
