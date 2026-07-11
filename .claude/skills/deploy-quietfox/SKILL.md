---
name: deploy-quietfox
description: Deploy the aws-p branch to the kiro-rs-quietfox container on the quietfox server (43.156.115.199 / quietfox.sbs). Use whenever the user asks to deploy, redeploy, ship, or roll out the aws-p branch / a new kiro-rs version to quietfox, or to update the quietfox container. Handles push, CI build-wait, pull-by-SHA (avoids stale ghcr cache), container restart, three-way digest verification, and health check. Supports new versions (uses current HEAD SHA) and a changeable host port.
---

# deploy-quietfox

固化「**aws-p 分支 → kiro-rs-quietfox 容器**」的部署流程。核心事实:

| 项 | 值 |
|---|---|
| 分支 | `aws-p`(仓库 `github.com/lusya123/kiro.rs`) |
| 容器 | `kiro-rs-quietfox` |
| 镜像 | `ghcr.io/lusya123/kiro-rs:aws-p-beta-<sha6>`(CI 按 commit SHA 前 6 位打 tag) |
| 服务器 | `ubuntu@43.156.115.199`(对外经反代到 quietfox.sbs) |
| 默认端口 | 宿主机 `8990` → 容器内 `8990` |
| 配置卷 | `/home/ubuntu/kiro.rs-quietfox/config` → `/app/config` |

## 怎么用

绝大多数情况直接跑脚本即可(它会:等 CI 构建 → **先 `docker logout ghcr.io` 避开陈旧缓存** → 按 SHA tag 拉镜像 → 重建容器 → 三方 digest 校验 → 健康检查):

```bash
bash .claude/skills/deploy-quietfox/deploy.sh
```

- **代码还没推**:加 `--push`,脚本会先 `git push origin aws-p` 再等构建。
- **镜像已构建好、只想重部署**:加 `--no-build-wait`。
- **换端口**:`--port 9110`(容器内部仍是 8990,只改宿主机映射)。
- **换容器名 / 服务器 / 配置目录**:`--container kiro-rs-foo --server ubuntu@1.2.3.4 --config-dir /path/config`。
- **部署某个历史 SHA**:`--sha 0c79c9`。
- **鉴权健康检查**:`--health-key sk-quietfox-...`(否则用无鉴权探活,401 也算已起)。

完整参数见 `deploy.sh --help`。

## 执行时要点(给助手)

1. 运行脚本后,**如实汇报**:分支、SHA、镜像 tag、容器名、端口、三方 digest 是否一致、健康检查 HTTP 码。
2. 若「构建未成功」或「digest 不一致」——**停下报告,不要谎报部署成功**。
3. 部署会**重启生产容器**(短暂中断真实用户),属于对外操作;除非用户已授权本次部署,否则先确认再跑。
4. 新版本无需改脚本:它默认取**当前 HEAD 的短 SHA**,推送并构建后即对应新的 `aws-p-beta-<sha6>`。
5. 不要把 SSH 密码写进任何被提交的文件;凭据从 `$SSHPASS` 或同目录 `.deploy-secret`(已 gitignore)读取。

## 凭据

SSH 密码从环境变量 `$SSHPASS` 读;没有则读 `.claude/skills/deploy-quietfox/.deploy-secret`(该文件已被本目录 `.gitignore` 忽略,不会进版本库)。换服务器/改密码时更新该文件即可。
