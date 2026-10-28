#!/bin/sh
# restore.sh —— 【还原】入口（内部调用同目录 release.sh --restore）
#   sh restore.sh                     交互式：列出所有还原点，选编号恢复
#   sh restore.sh 20261027-215844     直接还原到指定还原点
#   sh restore.sh --dry-run           只看有哪些还原点
# 只还原包并重跑各目标的 restart.sh；数据库的导入用 backup-db.sh import 手动做。
exec bash "$(dirname "$0")/release.sh" --restore "$@"
