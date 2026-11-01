#!/bin/sh
# backup-sqlite.sh —— SQLite 数据库备份 / 导入（容器内或宿主机文件，同一套约定）
# ---------------------------------------------------------------------------
# 用法：
#   sh backup-sqlite.sh backup            备份（容器内走 sqlite3 .backup，热备安全）
#   sh backup-sqlite.sh import <文件>     覆盖回原路径（需确认；建议先停应用）
#   sh backup-sqlite.sh list              列出已有备份
# 约定：
#   - 备份文件：db-backups/<前缀>.YYYYMMDD-HHMMSS.db
#   - 默认保留最近 KEEP_BACKUPS=10 个，超出自动清理最老的
#   - 完整性校验：文件头 == "SQLite format 3"
# 说明：
#   - 容器模式下优先用 sqlite3 .backup（在线热备，不怕并发写）；
#     容器里没有 sqlite3 时自动退回 docker cp 直接拷文件（若开 WAL 请连 -wal/-shm 一起）
#   - 导入 = 原样覆盖回 SQLITE_DB_PATH / SQLITE_HOST_FILE，建议先停应用再导
# 集成：多库场景参考 README「多个数据库」；导入永远手动，--restore 不碰数据库
# 纯 POSIX sh。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可环境变量覆盖）=====
SQLITE_CONTAINER="${SQLITE_CONTAINER:-}"      # 容器名；留空 = 备份宿主机文件
SQLITE_DB_PATH="${SQLITE_DB_PATH:-/data/app.db}"   # 容器内 db 路径
SQLITE_HOST_FILE="${SQLITE_HOST_FILE:-}"      # 容器模式为空时使用的宿主机路径
SQLITE_NAME="${SQLITE_NAME:-app}"             # 备份文件名前缀
BACKUP_DIR="${BACKUP_DIR:-db-backups}"
KEEP_BACKUPS="${KEEP_BACKUPS:-10}"
FORCE="${FORCE:-0}"
# ===========================================

usage() {
  echo "用法：sh backup-sqlite.sh [backup | import <文件> | list]" >&2
  exit 1
}

verify_db() { # $1=文件
  [ -s "$1" ] || return 1
  [ "$(head -c 15 -- "$1" 2>/dev/null)" = "SQLite format 3" ]
}

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
  bk_f="$BACKUP_DIR/$SQLITE_NAME.$(date +%Y%m%d-%H%M%S).db"
  echo "== 备份 SQLite -> $bk_f =="
  if [ -n "$SQLITE_CONTAINER" ]; then
    # 优先在线热备；容器里没有 sqlite3 则直接拷文件（WAL 模式下建议连 -wal/-shm 一起）
    if ! docker exec "$SQLITE_CONTAINER" sh -c \
        "sqlite3 '$SQLITE_DB_PATH' '.backup /tmp/release_snap.db'" 2>/dev/null; then
      echo "（容器内没有 sqlite3，退回直接拷贝文件）"
      docker cp "$SQLITE_CONTAINER:$SQLITE_DB_PATH" "$bk_f"
    else
      docker cp "$SQLITE_CONTAINER:/tmp/release_snap.db" "$bk_f"
      docker exec "$SQLITE_CONTAINER" rm -f /tmp/release_snap.db
    fi
  else
    [ -n "$SQLITE_HOST_FILE" ] || { echo "backup-sqlite.sh：SQLITE_CONTAINER 和 SQLITE_HOST_FILE 至少配一个" >&2; exit 1; }
    cp -p -- "$SQLITE_HOST_FILE" "$bk_f"
  fi
  if verify_db "$bk_f"; then
    echo "完成（$(du -h "$bk_f" | cut -f1)）"
    echo "BACKED_UP: $bk_f"
    prune_files "$SQLITE_NAME" ".db"
  else
    rm -f -- "$bk_f"
    echo "backup-sqlite.sh：备份不完整（不是有效的 SQLite 文件），已删除" >&2
    exit 1
  fi
}

do_import() { # $1=备份文件
  im_f="$1"
  [ -f "$im_f" ] || { echo "backup-sqlite.sh：找不到备份文件 $im_f" >&2; exit 1; }
  verify_db "$im_f" || { echo "backup-sqlite.sh：不是有效的 SQLite 文件，拒绝导入：$im_f" >&2; exit 1; }
  im_target="$SQLITE_DB_PATH"
  [ -n "$SQLITE_CONTAINER" ] && im_target="$SQLITE_CONTAINER:$SQLITE_DB_PATH"
  [ -z "$SQLITE_CONTAINER" ] && im_target="$SQLITE_HOST_FILE"
  if [ "$FORCE" != "1" ]; then
    if [ -t 0 ]; then
      printf '将把 %s 覆盖回 %s（现有数据丢失！建议先停应用）。确认请输入 yes：' \
        "$(basename "$im_f")" "$im_target"
      read -r im_reply
      [ "$im_reply" = "yes" ] || { echo "已取消"; exit 1; }
    else
      echo "backup-sqlite.sh：非交互终端不执行导入。确认无误后加 FORCE=1：FORCE=1 sh backup-sqlite.sh import $im_f" >&2
      exit 1
    fi
  fi
  echo "== 导入 $(basename "$im_f") =="
  if [ -n "$SQLITE_CONTAINER" ]; then
    docker cp "$im_f" "$SQLITE_CONTAINER:$SQLITE_DB_PATH"
  else
    cp -p -- "$im_f" "$SQLITE_HOST_FILE"
  fi
  echo "导入完成"
}

do_list() {
  ls -lh -- "$BACKUP_DIR" 2>/dev/null | grep -E '\.db$' || echo "（$BACKUP_DIR/ 里还没有 SQLite 备份）"
}

bk_cmd="${1:-backup}"
case "$bk_cmd" in
  backup) do_backup ;;
  import) shift; [ -n "${1:-}" ] || usage; do_import "$1" ;;
  list)   do_list ;;
  *)      usage ;;
esac
