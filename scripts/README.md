# scripts/ —— 正式版发布脚本

服务器上的目录布局（release.sh 与 sources/、targets/、manifest.txt 同级）：

```
deploy-root/
├── deploy.sh         发布入口（= release.sh，参数透传）
├── backup.sh         备份入口（= release.sh --backup）
├── restore.sh        还原入口（= release.sh --restore）
├── release.sh        主脚本（引擎，可单独使用）
├── manifest.txt      发布清单（路由表，一次写全、平时不动；删掉则回退目录约定模式）
├── backup-db.sh      数据库备份钩子（可选；从 templates/ 挑一个复制改名）
├── db-backups/       数据库备份存放目录（钩子自动创建）
├── sources/          上传区：每次只传改动的包，传多少发多少
└── targets/<目标名>/  每个目录一个部署目标：当前包 + restart.sh + 配置文件 + 还原点
```

## 从零搭建（首次部署，10 分钟）

1. **选位置**：在服务器上建部署根目录，建议放数据盘，如 `/data/deploy`
   （快照和数据库备份会持续占空间，别放系统盘、别放 web 根目录里；
   本套脚本用"自己所在目录"定位一切，放任意路径都能跑）。
   目录链路上不要有带空格的名字，不要用 root 日常运行。

2. **拷脚本**：把本目录的 `release.sh`、`deploy.sh`、`backup.sh`、`restore.sh`
   拷进部署根目录，`chmod +x`；要跑回归测试的话把 `selftest.sh` 也带上。

3. **建目录**：

   ```bash
   cd /data/deploy
   mkdir -p sources targets/xx-system targets/web
   ```

   每个目标目录 = 一个部署目的地，里面放：从 `templates/` 挑的 restart.sh
   （复制后改名为 `restart.sh`，改顶部【按项目改】）+ 配置文件（如 application.yml）。
   `sources/` 先建空的；`logs/`、`db-backups/` 会自动创建。

4. **写清单**：在部署根目录建 `manifest.txt`，把所有包写全（一次写全、平时不动）：

   ```
   xx-system-server.jar     xx-system
   web/dist.tar.gz          web
   ```

5. **验证**：

   ```bash
   bash selftest.sh          # （可选）服务器上跑一遍回归，环境没问题会全绿
   bash deploy.sh --dry-run  # 演练，检查分发计划
   bash deploy.sh            # 正式发布
   ```

6. **接数据库**（可选）：把 `templates/backup-mysql.sh` 或 `backup-pgsql.sh`
   复制到部署根目录改名为 `backup-db.sh`，改顶部容器名/账号/库名，
   之后 `bash backup.sh` 就会连数据库一起备。

**磁盘容量估算**：每个包留 10 个还原点（约 10 × 包大小）+ 数据库备份
（10 × 单次 dump 大小）× 库数，一般给 5–10GB 富余即可；不够就把
`KEEP_BACKUPS` 调小或定期人工清理 `targets/*/` 里的老还原点。

**Windows 上传注意**：scp/WinSCP 整目录拷贝即可（仓库里 `*.sh` 已强制 LF）；
千万别用记事本打开脚本再保存。传完可跑 `sed -i 's/\r$//' *.sh` 兜底 + `bash selftest.sh` 验证。

## 三个入口（加班时闭着眼选对）

| 命令 | 干什么 |
|------|--------|
| `sh deploy.sh` | 发布 sources/ 里的包（先 `--dry-run` 演练更稳） |
| `sh backup.sh` | 备份：各目标线上包快照 + 数据库（如有 backup-db.sh） |
| `sh restore.sh` | 还原：交互式选还原点恢复包并重跑 restart.sh |

数据库**导入永远手动**：`sh backup-db.sh import db-backups/appdb.20261027-215844.sql.gz`
（交互确认；脚本环境加 `FORCE=1`）。restore.sh 只回滚包，不碰数据库。

## 带 SQL 变更的发版流程（推荐）

```
1. sh deploy.sh --dry-run && sh deploy.sh
      发布会自动：包快照 + 数据库原始快照（有 backup-db.sh 时），
      并在结束时打印"数据回滚命令"备用
2. 执行本次新增的 SQL（手工；跑完 SQL 再测）
3. 测试
4. 通过   → sh backup.sh        存档新 SQL 版本的快照，收工
5. 不通过 → 两步回滚到上一个版本：
      数据：sh backup-db.sh import <第 1 步打印的发布前快照>
      代码：sh restore.sh  （选发布前的包还原点）
```

数据库快照失败会直接中止发布（防"SQL 上了回不去"）；
确需跳过数据库强发：`DB_HOOK_SOFT=1 sh deploy.sh`。

## 数据库备份钩子（backup-db.sh）

把 `templates/backup-mysql.sh`（docker MySQL）或 `templates/backup-pgsql.sh`
（docker PostgreSQL）复制为 deploy 根目录的 `backup-db.sh`，改顶部【按项目改】
（容器名/账号/库名），`sh backup.sh` 就会自动带上数据库：

- 备份：`db-backups/<库名>.YYYYMMDD-HHMMSS.sql.gz`（MySQL）/ `.dump`（PG 自定义格式），
  追加式不覆盖，每库默认保留 10 个（KEEP_BACKUPS 可调）
- 完整性校验：MySQL 查 mysqldump 收尾标志；PG 用 `pg_restore --list` 读归档目录；
  不完整的备份立即删除并报错
- 也可单独 cron 定时：`10 3 * * * cd /deploy && sh backup-db.sh backup`

## 还原点（备份）机制

- 每次发布会把当前线上包**追加**存成还原点 `包名.YYYYMMDD-HHMMSS`（不覆盖）：
  熬夜连发 10 版就有 10 个还原点，跨天也不会把调试版"占"成安全版；
  每包默认保留最近 10 个（`KEEP_BACKUPS` 可调），超出自动清理最老的
- 发布时 restart.sh 失败 → 自动用**本次发布前**的还原点恢复并重跑
- 手动命令：
  - `bash release.sh --backup` 手动快照各目标线上包（需 manifest.txt）
  - `bash release.sh --restore` 交互式列出还原点选一个恢复并重跑 sh；
    脚本环境用 `--restore 20261027-215844` 直接指定

## restart.sh 契约（release.sh 与技术栈无关的关键）

release.sh 对 restart.sh 只有三条约定，满足即可对接任何部署方式：

1. **调用**：`cd targets/<目标名>/` 后 `sh restart.sh <包名>...`。默认把包名作为
   参数传入；**脚本自己的 `$1` 有其他用途（如 stop|start 子命令）的，在
   `targets/<目标名>/.deploy-script` 第 2 行写 `no-args` 改为不传参数**
2. **退出码**：`0` = 成功；**非 0 = release.sh 自动回滚包到上一版本并重跑本脚本**
3. **定位**：脚本按自己所在目录定位目标目录（`dirname $0`），手动执行时
   在任何目录都能跑：`sh targets/xx-system/restart.sh`

## templates/ 按项目挑一个复制为 targets/<目标名>/restart.sh

| 模板 | 适用 | 要点 |
|------|------|------|
| `restart-docker.sh` | jar + docker 挂载式 | 删旧容器 → `docker run -v` 挂 jar（不构建镜像）；可选 HEALTH_URL 探测 |
| `restart-java.sh`   | jar + 裸进程 nohup | 停旧进程（PID 文件 + pgrep 兜底）→ `nohup java -jar`；可选 HEALTH_URL 探测 |
| `restart-static.sh` | dist.tar.gz 静态前端 | 解压到 WEB_ROOT 并检查 index.html；解压前自动备份现有站点 |
| `backup-mysql.sh`   | docker MySQL 备份/导入 | 复制为根目录 backup-db.sh；--backup 自动带上；也可单独 cron |
| `backup-pgsql.sh`   | docker PostgreSQL 备份/导入 | 同上，pg_dump 自定义格式 + pg_restore --list 校验 |

- 三个模板顶部都有【按项目改】参数块，**均可被环境变量覆盖**（不改脚本临时调参），
  如 `WEB_ROOT=/data/site sh restart.sh dist.tar.gz`
- 全部 POSIX sh 语法（`sh restart.sh` 在 dash 等环境也能跑）；
  失败路径都会打最近日志再 `exit 1`，触发 release.sh 回滚
- 不在上面三种里的（如 systemd）：自己写一个 restart.sh 遵守契约即可，例如：

  ```sh
  #!/bin/sh
  cp /path/to/targets/xx-system/xx-system-server.jar /opt/app/app.jar
  systemctl restart xx-system
  systemctl is-active --quiet xx-system || exit 1
  ```

## 常用命令

```bash
bash release.sh --dry-run                # 演练：只打印分发计划，不改任何文件
bash release.sh                          # 正式发布（自动快照当前线上包）
bash release.sh --backup                 # 手动快照线上包 + 数据库钩子
bash release.sh --restore                # 交互式还原（或 --restore <时间戳>）
bash selftest.sh                         # 改完 release.sh/模板后跑全场景回归（73 项断言）
```
