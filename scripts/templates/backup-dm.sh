#!/bin/sh
# backup-dm.sh —— docker 达梦 DM8 备份 / 导入（与 release.sh 还原点同一套约定）
# ---------------------------------------------------------------------------
# 用法：
#   sh backup-dm.sh backup            按 DM_SCHEMA 导出（dexp）
#   sh backup-dm.sh import <文件>     导入某个备份（dimp，TABLE_EXISTS_ACTION 覆盖，需确认）
#   sh backup-dm.sh list              列出已有备份
# 约定：
#   - 备份文件：db-backups/<模式名>.YYYYMMDD-HHMMSS.dmp
#   - 每模式默认保留最近 KEEP_BACKUPS=10 个，超出自动清理最老的
#   - 校验：dexp 退出码 + 文件非空（DM 无统一文件 magic；重要环境请先在测试库演练导入）
# 说明（务必按你的 DM 镜像调整）：
#   - 不同镜像的 dmdba 安装目录、工具路径不同：核对 DM_BIN / DM_IN_DIR
#   - DM_IN_DIR 必须是容器内已存在、可写的目录；密码含单引号会破坏拼接，请换密码
#   - dexp/dimp 用户名密码写法：'USER'/'PASS'@localhost:PORT
#   - 版本差异用 DM_EXP_ARGS / DM_IMP_ARGS 兜底追加（如 COMPRESS=Y、企版 PARALLEL=4）
# 集成：多库场景参考 README「多个数据库」；导入永远手动，--restore 不碰数据库
# 纯 POSIX sh。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可环境变量覆盖）=====
DM_CONTAINER="${DM_CONTAINER:-dm8}"
DM_USER="${DM_USER:-SYSDBA}"
DM_PASS="${DM_PASS:-SYSDBA001}"
DM_PORT="${DM_PORT:-5236}"
DM_BIN="${DM_BIN:-/opt/dmdbms/bin}"            # 容器内 dexp/dimp 所在目录
DM_IN_DIR="${DM_IN_DIR:-/tmp}"                 # 容器内暂存目录（须已存在、可写）
DM_SCHEMA="${DM_SCHEMA:-APPSHEMA}"             # 要备份的模式（一次一个）
DM_EXP_ARGS="${DM_EXP_ARGS:-}"                 # 附加导出参数（版本相关：如 COMPRESS=Y；企业版才支持 PARALLEL=N）
DM_IMP_ARGS="${DM_IMP_ARGS:-}"                 # 附加导入参数（同样按镜像/版本调整）
BACKUP_DIR="${BACKUP_DIR:-db-backups}"
KEEP_BACKUPS="${KEEP_BACKUPS:-10}"
FORCE="${FORCE:-0}"
# ===========================================

usage() {
  echo "用法：sh backup-dm.sh [backup | import <文件> | list]" >&2
  exit 1
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
  bk_f="$BACKUP_DIR/$DM_SCHEMA.$(date +%Y%m%d-%H%M%S).dmp"
  echo "== 备份模式 $DM_SCHEMA -> $bk_f =="
  if ! docker exec "$DM_CONTAINER" sh -c \
      "cd '$DM_IN_DIR' && rm -f dmexp_tmp.dmp dmexp_tmp.log && '$DM_BIN/dexp' '$DM_USER'/'$DM_PASS'@localhost:$DM_PORT FILE=dmexp_tmp.dmp LOG=dmexp_tmp.log DIRECTORY='$DM_IN_DIR' SCHEMAS='$DM_SCHEMA' $DM_EXP_ARGS"; then
    echo "backup-dm.sh：dexp 导出失败，进容器看 $DM_IN_DIR/dmexp_tmp.log" >&2
    exit 1
  fi
  docker cp "$DM_CONTAINER:$DM_IN_DIR/dmexp_tmp.dmp" "$bk_f"
  docker exec "$DM_CONTAINER" rm -f "$DM_IN_DIR/dmexp_tmp.dmp" "$DM_IN_DIR/dmexp_tmp.log"
  if [ -s "$bk_f" ]; then
    echo "完成（$(du -h "$bk_f" | cut -f1)）"
    echo "BACKED_UP: $bk_f"
    prune_files "$DM_SCHEMA" ".dmp"
  else
    rm -f -- "$bk_f"
    echo "backup-dm.sh：导出文件为空，已删除" >&2
    exit 1
  fi
}

do_import() { # $1=备份文件
  im_f="$1"
  [ -f "$im_f" ] || { echo "backup-dm.sh：找不到备份文件 $im_f" >&2; exit 1; }
  [ -s "$im_f" ] || { echo "backup-dm.sh：备份文件为空，拒绝导入：$im_f" >&2; exit 1; }
  if [ "$FORCE" != "1" ]; then
    if [ -t 0 ]; then
      printf '将把 %s 导入模式 %s（覆盖同名表数据！）。确认请输入 yes：' "$(basename "$im_f")" "$DM_SCHEMA"
      read -r im_reply
      [ "$im_reply" = "yes" ] || { echo "已取消"; exit 1; }
    else
      echo "backup-dm.sh：非交互终端不执行导入。确认无误后加 FORCE=1：FORCE=1 sh backup-dm.sh import $im_f" >&2
      exit 1
    fi
  fi
  echo "== 导入 $(basename "$im_f") -> 模式 $DM_SCHEMA =="
  docker cp "$im_f" "$DM_CONTAINER:$DM_IN_DIR/dmimp_tmp.dmp"
  if ! docker exec "$DM_CONTAINER" sh -c \
      "'$DM_BIN/dimp' '$DM_USER'/'$DM_PASS'@localhost:$DM_PORT FILE='$DM_IN_DIR/dmimp_tmp.dmp' LOG=dmimp_tmp.log DIRECTORY='$DM_IN_DIR' SCHEMAS='$DM_SCHEMA' TABLE_EXISTS_ACTION=REPLACE $DM_IMP_ARGS"; then
    echo "backup-dm.sh：dimp 导入报错，进容器看 $DM_IN_DIR/dmimp_tmp.log；数据可能处于中间状态，可重新导入" >&2
    exit 1
  fi
  docker exec "$DM_CONTAINER" rm -f "$DM_IN_DIR/dmimp_tmp.dmp" "$DM_IN_DIR/dmimp_tmp.log"
  echo "导入完成：$DM_SCHEMA"
}

do_list() {
  ls -lh -- "$BACKUP_DIR" 2>/dev/null | grep -E '\.dmp$' || echo "（$BACKUP_DIR/ 里还没有达梦备份）"
}

bk_cmd="${1:-backup}"
case "$bk_cmd" in
  backup) do_backup ;;
  import) shift; [ -n "${1:-}" ] || usage; do_import "$1" ;;
  list)   do_list ;;
  *)      usage ;;
esac
