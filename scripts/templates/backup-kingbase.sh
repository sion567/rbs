#!/bin/sh
# backup-kingbase.sh —— docker 人大金仓 KingbaseES 备份 / 导入（与 release.sh 还原点同一套约定）
# ---------------------------------------------------------------------------
# 用法：
#   sh backup-kingbase.sh backup            备份 DBS 里所有库
#   sh backup-kingbase.sh import <文件>     恢复某个备份（--clean 覆盖，需确认）
#   sh backup-kingbase.sh list              列出已有备份
# 约定：
#   - 备份文件：db-backups/<库名>.YYYYMMDD-HHMMSS.dump（sys_dump -Fc 自定义格式）
#   - 每库默认保留最近 KEEP_BACKUPS=10 个，超出自动清理最老的
#   - 完整性校验：sys_restore --list 能完整读出归档目录（容器内执行，stdin 喂入）
#   - 导入用 sys_restore --clean --if-exists：先删旧对象再建，目标库需已存在
#   - dump 带 --no-owner：跨实例恢复不因缺角色报错（同实例恢复无副作用）
# 说明：
#   - 金仓基于 PostgreSQL，sys_dump/sys_restore 与 pg_dump/pg_restore 同源；
#     密码环境变量多数镜像兼容 PGPASSWORD，个别镜像要求 KB_PASSWORD，按实际调整
#   - 默认超级用户是 system（不是 postgres）
# 集成：多库场景参考 README「多个数据库」；导入永远手动，--restore 不碰数据库
# 纯 POSIX sh。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可环境变量覆盖）=====
KBS_CONTAINER="${KBS_CONTAINER:-kingbase}"    # kingbase 容器名
KBS_USER="${KBS_USER:-system}"
KBS_PASS="${KBS_PASS:-}"                      # 建议环境变量传入
DBS="${DBS:-appdb}"
BACKUP_DIR="${BACKUP_DIR:-db-backups}"
KEEP_BACKUPS="${KEEP_BACKUPS:-10}"
FORCE="${FORCE:-0}"
KBS_RESTORE_JOBS="${KBS_RESTORE_JOBS:-}"      # 并行导入线程数（如 4，空=串行；仅 -Fc 自定义格式支持）
# ===========================================

usage() {
  echo "用法：sh backup-kingbase.sh [backup | import <文件> | list]" >&2
  exit 1
}

verify_dump() { # $1=文件
  docker exec -i "$KBS_CONTAINER" sys_restore --list < "$1" >/dev/null 2>&1
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
  bk_fail=0
  for db in $DBS; do
    bk_f="$BACKUP_DIR/$db.$(date +%Y%m%d-%H%M%S).dump"
    echo "== 备份 $db -> $bk_f =="
    # -Fc 自定义格式（压缩）；--no-owner 跨实例恢复不因缺角色报错
    if docker exec -e PGPASSWORD="$KBS_PASS" "$KBS_CONTAINER" \
        sys_dump -U "$KBS_USER" -Fc --no-owner -d "$db" > "$bk_f" && verify_dump "$bk_f"; then
      echo "完成（$(du -h "$bk_f" | cut -f1)）"
      echo "BACKED_UP: $bk_f"
      prune_files "$db" ".dump"
    else
      rm -f -- "$bk_f"
      echo "backup-kingbase.sh：$db 备份失败或不完整（sys_restore 无法读取），已删除" >&2
      bk_fail=1
    fi
  done
  [ "$bk_fail" -eq 0 ] || exit 1
  echo "数据库备份完成：$BACKUP_DIR/"
}

do_import() { # $1=备份文件
  im_f="$1"
  [ -f "$im_f" ] || { echo "backup-kingbase.sh：找不到备份文件 $im_f" >&2; exit 1; }
  verify_dump "$im_f" || { echo "backup-kingbase.sh：备份不完整/损坏，拒绝导入：$im_f" >&2; exit 1; }
  im_base="$(basename "$im_f")"; im_base="${im_base%.dump}"
  im_db="${im_base%.*}"
  case "$im_db" in
    ''|*.*) echo "backup-kingbase.sh：无法从文件名解析库名（应为 <库名>.YYYYMMDD-HHMMSS.dump）：$im_f" >&2; exit 1 ;;
  esac
  im_rjobs=""
  if [ -n "$KBS_RESTORE_JOBS" ]; then im_rjobs="-j $KBS_RESTORE_JOBS"; fi
  if [ "$FORCE" != "1" ]; then
    if [ -t 0 ]; then
      printf '将把 %s 恢复到库 %s（--clean 先删现有对象！）。确认请输入 yes：' "$(basename "$im_f")" "$im_db"
      read -r im_reply
      [ "$im_reply" = "yes" ] || { echo "已取消"; exit 1; }
    else
      echo "backup-kingbase.sh：非交互终端不执行导入。确认无误后加 FORCE=1：FORCE=1 sh backup-kingbase.sh import $im_f" >&2
      exit 1
    fi
  fi
  echo "== 导入 $(basename "$im_f") -> $im_db =="
  if ! docker exec -i -e PGPASSWORD="$KBS_PASS" "$KBS_CONTAINER" \
      sys_restore -U "$KBS_USER" --clean --if-exists $im_rjobs -d "$im_db" < "$im_f"; then
    echo "backup-kingbase.sh：sys_restore 报错，数据可能处于中间状态，可重新导入" >&2
    exit 1
  fi
  echo "导入完成：$im_db"
}

do_list() {
  ls -lh -- "$BACKUP_DIR" 2>/dev/null | grep -E '\.dump$' || echo "（$BACKUP_DIR/ 里还没有金仓备份）"
}

bk_cmd="${1:-backup}"
case "$bk_cmd" in
  backup) do_backup ;;
  import) shift; [ -n "${1:-}" ] || usage; do_import "$1" ;;
  list)   do_list ;;
  *)      usage ;;
esac
