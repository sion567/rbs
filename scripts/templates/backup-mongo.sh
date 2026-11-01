#!/bin/sh
# backup-mongo.sh —— docker MongoDB 备份 / 导入（与 release.sh 还原点同一套约定）
# ---------------------------------------------------------------------------
# 用法：
#   sh backup-mongo.sh backup            备份 DBS 里所有库（不带参数等同 backup）
#   sh backup-mongo.sh import <文件>     导入某个备份（--drop 覆盖同名集合，需确认）
#   sh backup-mongo.sh list              列出已有备份
# 约定：
#   - 备份文件：db-backups/<库名>.YYYYMMDD-HHMMSS.archive.gz（mongodump --archive --gzip）
#   - 每库默认保留最近 KEEP_BACKUPS=10 个，超出自动清理最老的
#   - 完整性校验：mongorestore --dryRun 能完整读出归档（容器内执行，stdin 喂入）
#   - 导入用 --drop：先删同名集合再灌，恢复到备份时刻的库状态
#   - 副本集环境设 MONGO_OPLOG=1：备份记 oplog、恢复 --oplogReplay（单机勿开）
# 集成：
#   - 多库场景参考 README「多个数据库」：backup-db.sh 串联调用本脚本
#   - 数据库的导入永远手动执行：release.sh --restore 只回滚包，不碰数据库
# 纯 POSIX sh。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可环境变量覆盖）=====
MONGO_CONTAINER="${MONGO_CONTAINER:-mongo}"   # mongo 容器名
MONGO_USER="${MONGO_USER:-}"                  # 无认证就留空
MONGO_PASS="${MONGO_PASS:-}"
MONGO_AUTHDB="${MONGO_AUTHDB:-admin}"
DBS="${DBS:-appdb}"                           # 要备份的库，空格分隔多个
BACKUP_DIR="${BACKUP_DIR:-db-backups}"
KEEP_BACKUPS="${KEEP_BACKUPS:-10}"
FORCE="${FORCE:-0}"
MONGO_OPLOG="${MONGO_OPLOG:-0}"               # 副本集设 1：oplog 一致性备份+可重放恢复（单机勿开，会备份失败）
# ===========================================

usage() {
  echo "用法：sh backup-mongo.sh [backup | import <文件> | list]" >&2
  exit 1
}

# 认证参数（$MONGO_AUTH 不加引号展开是有意的；密码别带空格）
if [ -n "$MONGO_USER" ]; then
  MONGO_AUTH="-u $MONGO_USER -p $MONGO_PASS --authenticationDatabase $MONGO_AUTHDB"
else
  MONGO_AUTH=""
fi

# oplog 参数（副本集专用：备份期记录变更流，恢复重放到一致点；单机开启会备份失败）
MONGO_DUMP_OPLOG=""; MONGO_IMP_OPLOG=""
if [ "$MONGO_OPLOG" = "1" ]; then
  MONGO_DUMP_OPLOG="--oplog"
  MONGO_IMP_OPLOG="--oplogReplay"
fi

# 完整性：mongorestore 干跑能完整读出归档目录
verify_dump() { # $1=文件
  docker exec -i $MONGO_AUTH "$MONGO_CONTAINER" mongorestore --gzip --dryRun --archive < "$1" >/dev/null 2>&1
}

# 通用清理：$1=文件名前缀 $2=扩展名
prune_files() {
  pr_files="$(ls -1 "$BACKUP_DIR/$1."*"$2" 2>/dev/null | sort)" || true
  [ -n "$pr_files" ] || return 0
  pr_total="$(printf '%s\n' "$pr_files" | grep -c '')"
  pr_over=$((pr_total - KEEP_BACKUPS))
  [ "$pr_over" -gt 0 ] || return 0
  printf '%s\n' "$pr_files" | head -n "$pr_over" | while IFS= read -r f; do
    rm -f -- "$f"
    echo "清理最老备份：$f"
  done
}

do_backup() {
  mkdir -p "$BACKUP_DIR"
  bk_fail=0
  for db in $DBS; do
    bk_f="$BACKUP_DIR/$db.$(date +%Y%m%d-%H%M%S).archive.gz"
    echo "== 备份 $db -> $bk_f =="
    docker exec $MONGO_AUTH "$MONGO_CONTAINER" mongodump --db "$db" --archive --gzip $MONGO_DUMP_OPLOG > "$bk_f"
    if verify_dump "$bk_f" && [ -s "$bk_f" ]; then
      echo "完成（$(du -h "$bk_f" | cut -f1)）"
      echo "BACKED_UP: $bk_f"
      prune_files "$db" ".archive.gz"
    else
      rm -f -- "$bk_f"
      echo "backup-mongo.sh：$db 备份不完整（mongorestore 干跑失败），已删除" >&2
      bk_fail=1
    fi
  done
  [ "$bk_fail" -eq 0 ] || exit 1
  echo "数据库备份完成：$BACKUP_DIR/"
}

do_import() { # $1=备份文件
  im_f="$1"
  [ -f "$im_f" ] || { echo "backup-mongo.sh：找不到备份文件 $im_f" >&2; exit 1; }
  verify_dump "$im_f" || { echo "backup-mongo.sh：备份不完整/损坏，拒绝导入：$im_f" >&2; exit 1; }
  im_base="$(basename "$im_f")"; im_base="${im_base%.archive.gz}"
  im_db="${im_base%.*}"
  case "$im_db" in
    ''|*.*) echo "backup-mongo.sh：无法从文件名解析库名（应为 <库名>.YYYYMMDD-HHMMSS.archive.gz）：$im_f" >&2; exit 1 ;;
  esac
  if [ "$FORCE" != "1" ]; then
    if [ -t 0 ]; then
      printf '将把 %s 导回库 %s（--drop 覆盖现有集合！）。确认请输入 yes：' "$(basename "$im_f")" "$im_db"
      read -r im_reply
      [ "$im_reply" = "yes" ] || { echo "已取消"; exit 1; }
    else
      echo "backup-mongo.sh：非交互终端不执行导入。确认无误后加 FORCE=1：FORCE=1 sh backup-mongo.sh import $im_f" >&2
      exit 1
    fi
  fi
  echo "== 导入 $(basename "$im_f") -> $im_db =="
  docker exec -i $MONGO_AUTH "$MONGO_CONTAINER" mongorestore --gzip --drop --archive $MONGO_IMP_OPLOG < "$im_f"
  echo "导入完成：$im_db"
}

do_list() {
  ls -lh -- "$BACKUP_DIR" 2>/dev/null | grep -E '\.archive\.gz' || echo "（$BACKUP_DIR/ 里还没有 MongoDB 备份）"
}

bk_cmd="${1:-backup}"
case "$bk_cmd" in
  backup) do_backup ;;
  import) shift; [ -n "${1:-}" ] || usage; do_import "$1" ;;
  list)   do_list ;;
  *)      usage ;;
esac
