#!/bin/sh
# backup-mssql.sh —— docker SQL Server 备份 / 导入（与 release.sh 还原点同一套约定）
# ---------------------------------------------------------------------------
# 用法：
#   sh backup-mssql.sh backup            备份 DBS 里所有库
#   sh backup-mssql.sh import <文件>     恢复某个备份（WITH REPLACE 覆盖，需确认）
#   sh backup-mssql.sh list              列出已有备份
# 约定：
#   - 备份文件：db-backups/<库名>.YYYYMMDD-HHMMSS.bak（原生 BACKUP DATABASE）
#   - 每库默认保留最近 KEEP_BACKUPS=10 个，超出自动清理最老的
#   - 完整性校验：sqlcmd 输出含 "processed"（BACKUP/RESTORE 的页处理回执）且文件非空
#   - 备份先落容器内 $MSSQL_TMP，再 docker cp 拷出，用完即删
# 说明：
#   - SQL Server 2017/2019 镜像工具在 /opt/mssql-tools/bin；
#     2022 新镜像在 /opt/mssql-tools18/bin 且需要 -C（信任自签证书）：
#     SQLTOOLS_BIN=/opt/mssql-tools18/bin SQLTOOLS_ARGS="-C" sh backup-mssql.sh backup
# 集成：多库场景参考 README「多个数据库」；导入永远手动，--restore 不碰数据库
# 纯 POSIX sh。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可环境变量覆盖）=====
MSSQL_CONTAINER="${MSSQL_CONTAINER:-mssql}"       # mssql 容器名
MSSQL_USER="${MSSQL_USER:-sa}"
MSSQL_PASS="${MSSQL_PASS:-}"                      # 必填
MSSQL_TMP="${MSSQL_TMP:-/var/opt/mssql/release_backup.bak}"   # 容器内暂存路径
SQLTOOLS_BIN="${SQLTOOLS_BIN:-/opt/mssql-tools/bin}"
SQLTOOLS_ARGS="${SQLTOOLS_ARGS:-}"                # 如 18 版镜像要加 "-C"
DBS="${DBS:-appdb}"
BACKUP_DIR="${BACKUP_DIR:-db-backups}"
KEEP_BACKUPS="${KEEP_BACKUPS:-10}"
FORCE="${FORCE:-0}"
# ===========================================

usage() {
  echo "用法：sh backup-mssql.sh [backup | import <文件> | list]" >&2
  exit 1
}

[ -n "$MSSQL_PASS" ] || { echo "backup-mssql.sh：请用环境变量提供 MSSQL_PASS" >&2; exit 1; }

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
    bk_f="$BACKUP_DIR/$db.$(date +%Y%m%d-%H%M%S).bak"
    echo "== 备份 $db -> $bk_f =="
    # CHECKSUM 写入页校验和：RESTORE VERIFYONLY 可离线校验备份完整性
    # （恢复侧不带 WITH CHECKSUM，兼容本改动之前生成的无校验和旧备份）
    if ! bk_out="$(docker exec "$MSSQL_CONTAINER" "$SQLTOOLS_BIN/sqlcmd" $SQLTOOLS_ARGS \
        -S localhost -U "$MSSQL_USER" -P "$MSSQL_PASS" \
        -Q "BACKUP DATABASE [$db] TO DISK='$MSSQL_TMP' WITH INIT, COMPRESSION, CHECKSUM")"; then
      echo "backup-mssql.sh：$db BACKUP 命令失败，输出如下" >&2
      printf '%s\n' "$bk_out" >&2
      bk_fail=1
      continue
    fi
    docker cp "$MSSQL_CONTAINER:$MSSQL_TMP" "$bk_f"
    docker exec "$MSSQL_CONTAINER" rm -f "$MSSQL_TMP"
    if [ -s "$bk_f" ] && printf '%s' "$bk_out" | grep -q "processed"; then
      echo "完成（$(du -h "$bk_f" | cut -f1)）"
      echo "BACKED_UP: $bk_f"
      prune_files "$db" ".bak"
    else
      rm -f -- "$bk_f"
      echo "backup-mssql.sh：$db 备份不完整（无 processed 回执），已删除" >&2
      bk_fail=1
    fi
  done
  [ "$bk_fail" -eq 0 ] || exit 1
  echo "数据库备份完成：$BACKUP_DIR/"
}

do_import() { # $1=备份文件
  im_f="$1"
  [ -f "$im_f" ] || { echo "backup-mssql.sh：找不到备份文件 $im_f" >&2; exit 1; }
  [ -s "$im_f" ] || { echo "backup-mssql.sh：备份文件为空，拒绝导入：$im_f" >&2; exit 1; }
  im_base="$(basename "$im_f")"; im_base="${im_base%.bak}"
  im_db="${im_base%.*}"
  case "$im_db" in
    ''|*.*) echo "backup-mssql.sh：无法从文件名解析库名（应为 <库名>.YYYYMMDD-HHMMSS.bak）：$im_f" >&2; exit 1 ;;
  esac
  if [ "$FORCE" != "1" ]; then
    if [ -t 0 ]; then
      printf '将把 %s 恢复到库 %s（WITH REPLACE 覆盖现有库！）。确认请输入 yes：' "$(basename "$im_f")" "$im_db"
      read -r im_reply
      [ "$im_reply" = "yes" ] || { echo "已取消"; exit 1; }
    else
      echo "backup-mssql.sh：非交互终端不执行导入。确认无误后加 FORCE=1：FORCE=1 sh backup-mssql.sh import $im_f" >&2
      exit 1
    fi
  fi
  echo "== 导入 $(basename "$im_f") -> $im_db =="
  docker cp "$im_f" "$MSSQL_CONTAINER:$MSSQL_TMP"
  if ! im_out="$(docker exec "$MSSQL_CONTAINER" "$SQLTOOLS_BIN/sqlcmd" $SQLTOOLS_ARGS \
      -S localhost -U "$MSSQL_USER" -P "$MSSQL_PASS" \
      -Q "RESTORE DATABASE [$im_db] FROM DISK='$MSSQL_TMP' WITH REPLACE, RECOVERY")"; then
    printf '%s\n' "$im_out" >&2
    echo "backup-mssql.sh：RESTORE 失败，数据可能处于中间状态，可重新执行导入" >&2
    exit 1
  fi
  printf '%s' "$im_out" | grep -q "processed" || { echo "backup-mssql.sh：RESTORE 无 processed 回执，请人工核对" >&2; exit 1; }
  docker exec "$MSSQL_CONTAINER" rm -f "$MSSQL_TMP"
  echo "导入完成：$im_db"
}

do_list() {
  ls -lh -- "$BACKUP_DIR" 2>/dev/null | grep -E '\.bak$' || echo "（$BACKUP_DIR/ 里还没有 SQL Server 备份）"
}

bk_cmd="${1:-backup}"
case "$bk_cmd" in
  backup) do_backup ;;
  import) shift; [ -n "${1:-}" ] || usage; do_import "$1" ;;
  list)   do_list ;;
  *)      usage ;;
esac
