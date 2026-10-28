#!/bin/sh
# backup-pgsql.sh —— docker PostgreSQL 数据库备份 / 导入（与 release.sh 还原点同一套约定）
# ---------------------------------------------------------------------------
# 用法：
#   sh backup-pgsql.sh backup            备份 DBS 里所有库（不带参数等同 backup）
#   sh backup-pgsql.sh import <文件>     导入某个备份（危险：覆盖现有数据，需确认）
#   sh backup-pgsql.sh list              列出已有备份
# 约定：
#   - 备份文件：db-backups/<库名>.YYYYMMDD-HHMMSS.dump（pg_dump -Fc 自定义格式，
#     已压缩，可用 pg_restore --list 校验 / 选择性恢复）
#   - 每库默认保留最近 KEEP_BACKUPS=10 个，超出自动清理最老的
#   - 完整性校验：pg_restore --list 能完整读取归档目录；不完整立即删除并报错
#   - 导入用 pg_restore --clean --if-exists：先删旧对象再建，目标库需已存在
# 集成：
#   - 复制本文件为 deploy 根目录的 backup-db.sh，release.sh --backup 会自动执行它
#   - 也可单独 cron 定时：10 3 * * * cd /deploy && sh backup-db.sh backup
#   - 数据库的导入永远手动执行：release.sh --restore 只回滚包，不碰数据库
# 纯 POSIX sh。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可环境变量覆盖，如 PGS_CONTAINER=pg sh backup-db.sh backup）=====
PGS_CONTAINER="${PGS_CONTAINER:-postgres}"    # postgres 容器名（docker ps 里的名字）
PGS_USER="${PGS_USER:-postgres}"
DBS="${DBS:-appdb}"                           # 要备份的库，空格分隔多个
BACKUP_DIR="${BACKUP_DIR:-db-backups}"        # 备份目录（相对当前目录或绝对路径）
KEEP_BACKUPS="${KEEP_BACKUPS:-10}"
FORCE="${FORCE:-0}"                           # import 时跳过交互确认（非交互环境用）
# ===========================================

usage() {
  echo "用法：sh backup-db.sh [backup | import <文件> | list]" >&2
  exit 1
}

# 完整性：pg_restore 能完整读出自定义格式归档的目录
verify_dump() { # $1=文件
  docker exec -i "$PGS_CONTAINER" pg_restore --list < "$1" >/dev/null 2>&1
}

# 清理某库超出 KEEP_BACKUPS 的最老备份（文件名时间序即时间顺序）
prune_db() { # $1=库名
  db="$1"
  files="$(ls -1 "$BACKUP_DIR/$db."*.dump 2>/dev/null | sort)" || true
  [ -n "$files" ] || return 0
  total="$(printf '%s\n' "$files" | grep -c '')"
  over=$((total - KEEP_BACKUPS))
  [ "$over" -gt 0 ] || return 0
  printf '%s\n' "$files" | head -n "$over" | while IFS= read -r f; do
    rm -f -- "$f"
    echo "清理最老数据库备份：$f"
  done
}

do_backup() {
  mkdir -p "$BACKUP_DIR"
  bk_fail=0
  for db in $DBS; do
    bk_f="$BACKUP_DIR/$db.$(date +%Y%m%d-%H%M%S).dump"
    echo "== 备份 $db -> $bk_f =="
    if docker exec "$PGS_CONTAINER" pg_dump -U "$PGS_USER" -Fc -d "$db" > "$bk_f" \
       && verify_dump "$bk_f"; then
      echo "完成（$(du -h "$bk_f" | cut -f1)）"
      echo "BACKED_UP: $bk_f"
      prune_db "$db"
    else
      rm -f -- "$bk_f"
      echo "backup-db.sh：$db 备份失败或不完整（pg_restore 无法读取），已删除。排查：容器名 / 账号权限 / 磁盘" >&2
      bk_fail=1
    fi
  done
  [ "$bk_fail" -eq 0 ] || exit 1
  echo "数据库备份完成：$BACKUP_DIR/"
}

do_import() { # $1=备份文件
  im_f="$1"
  [ -f "$im_f" ] || { echo "backup-db.sh：找不到备份文件 $im_f" >&2; exit 1; }
  verify_dump "$im_f" || { echo "backup-db.sh：备份文件不完整/损坏，拒绝导入：$im_f" >&2; exit 1; }
  im_base="$(basename "$im_f")"; im_base="${im_base%.dump}"
  im_db="${im_base%.*}"
  case "$im_db" in
    ''|*.*) echo "backup-db.sh：无法从文件名解析库名（应为 <库名>.YYYYMMDD-HHMMSS.dump）：$im_f" >&2; exit 1 ;;
  esac
  if [ "$FORCE" != "1" ]; then
    if [ -t 0 ]; then
      printf '将把 %s 恢复到数据库 %s（--clean：先删现有对象！）。确认请输入 yes：' "$(basename "$im_f")" "$im_db"
      read -r im_reply
      [ "$im_reply" = "yes" ] || { echo "已取消"; exit 1; }
    else
      echo "backup-db.sh：非交互终端不执行导入。确认无误后加 FORCE=1：FORCE=1 sh backup-db.sh import $im_f" >&2
      exit 1
    fi
  fi
  echo "== 导入 $(basename "$im_f") -> $im_db =="
  docker exec -i "$PGS_CONTAINER" pg_restore -U "$PGS_USER" --clean --if-exists -d "$im_db" < "$im_f"
  echo "导入完成：$im_db"
}

do_list() {
  ls -lh -- "$BACKUP_DIR" 2>/dev/null | grep -E '\.dump$' || echo "（$BACKUP_DIR/ 里还没有数据库备份）"
}

bk_cmd="${1:-backup}"
case "$bk_cmd" in
  backup) do_backup ;;
  import) shift; [ -n "${1:-}" ] || usage; do_import "$1" ;;
  list)   do_list ;;
  *)      usage ;;
esac
