#!/bin/sh
# backup.sh —— 【备份】入口（内部调用同目录 release.sh --backup）
# 一次命令备份两样东西：
#   1. 各目标目录里的线上包 -> 追加式还原点（包名.YYYYMMDD-HHMMSS）
#   2. 数据库（若 deploy 根目录有 backup-db.sh 钩子，见 templates/backup-mysql.sh）
#   sh backup.sh              备份全部（manifest 里列的所有目标）
#   sh backup.sh --targets a  只备指定目标
#   sh backup.sh --dry-run    演练
exec bash "$(dirname "$0")/release.sh" --backup "$@"
