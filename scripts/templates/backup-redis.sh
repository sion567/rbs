#!/bin/sh
# backup-redis.sh —— docker Redis 备份 / 导入（与 release.sh 还原点同一套约定）
# ---------------------------------------------------------------------------
# 用法：
#   sh backup-redis.sh backup            SAVE 后拷出 RDB 快照
#   sh backup-redis.sh import <文件>     拷回 RDB 并重启容器加载（覆盖现有数据，需确认）
#   sh backup-redis.sh list              列出已有备份
# 约定：
#   - 备份文件：db-backups/<前缀>.YYYYMMDD-HHMMSS.rdb
#   - 每实例默认保留最近 KEEP_BACKUPS=10 个，超出自动清理最老的
#   - 完整性校验：RDB 文件头 5 字节 == "REDIS"
#   - 导入 = docker cp 回容器数据目录 + docker restart（Redis 启动时加载 dump.rdb）
# 说明：
#   - SAVE 是同步落盘（实例大会卡几秒），介意请改成 BGSAVE + 轮询 lastsave
#   - 只备 RDB 意味着丢失最后一次快照之后的数据；要更细粒度请开 AOF 并连 AOF 一起备
# 集成：多库场景参考 README「多个数据库」；导入永远手动，--restore 不碰数据库
# 纯 POSIX sh。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可环境变量覆盖）=====
REDIS_CONTAINER="${REDIS_CONTAINER:-redis}"   # redis 容器名
REDIS_PASS="${REDIS_PASS:-}"                  # 无密码留空
REDIS_DIR="${REDIS_DIR:-/data}"               # 容器内 dump.rdb 所在目录（dir 配置）
REDIS_RDB="${REDIS_RDB:-dump.rdb}"
REDIS_NAME="${REDIS_NAME:-redis}"             # 备份文件名前缀（多实例时区分）
BACKUP_DIR="${BACKUP_DIR:-db-backups}"
KEEP_BACKUPS="${KEEP_BACKUPS:-10}"
FORCE="${FORCE:-0}"
# ===========================================

usage() {
  echo "用法：sh backup-redis.sh [backup | import <文件> | list]" >&2
  exit 1
}

verify_rdb() { # $1=文件
  [ -s "$1" ] || return 1
  [ "$(head -c 5 -- "$1" 2>/dev/null)" = "REDIS" ]
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

bk_cli() { # redis-cli 附加参数
  if [ -n "$REDIS_PASS" ]; then
    printf '%s' "-a $REDIS_PASS --no-auth-warning"
  fi
}

do_backup() {
  mkdir -p "$BACKUP_DIR"
  bk_f="$BACKUP_DIR/$REDIS_NAME.$(date +%Y%m%d-%H%M%S).rdb"
  echo "== 备份 $REDIS_CONTAINER（SAVE 落盘）-> $bk_f =="
  docker exec "$REDIS_CONTAINER" sh -c "redis-cli $(bk_cli) SAVE"
  docker cp "$REDIS_CONTAINER:$REDIS_DIR/$REDIS_RDB" "$bk_f"
  if verify_rdb "$bk_f"; then
    echo "完成（$(du -h "$bk_f" | cut -f1)）"
    echo "BACKED_UP: $bk_f"
    prune_files "$REDIS_NAME" ".rdb"
  else
    rm -f -- "$bk_f"
    echo "backup-redis.sh：备份不完整（不是有效的 RDB 文件），已删除。排查：容器内 $REDIS_DIR/$REDIS_RDB 是否存在" >&2
    exit 1
  fi
}

do_import() { # $1=备份文件
  im_f="$1"
  [ -f "$im_f" ] || { echo "backup-redis.sh：找不到备份文件 $im_f" >&2; exit 1; }
  verify_rdb "$im_f" || { echo "backup-redis.sh：不是有效的 RDB 文件，拒绝导入：$im_f" >&2; exit 1; }
  if [ "$FORCE" != "1" ]; then
    if [ -t 0 ]; then
      printf '将把 %s 覆盖到 %s:%s/%s 并重启容器（现有数据丢失！）。确认请输入 yes：' \
        "$(basename "$im_f")" "$REDIS_CONTAINER" "$REDIS_DIR" "$REDIS_RDB"
      read -r im_reply
      [ "$im_reply" = "yes" ] || { echo "已取消"; exit 1; }
    else
      echo "backup-redis.sh：非交互终端不执行导入。确认无误后加 FORCE=1：FORCE=1 sh backup-redis.sh import $im_f" >&2
      exit 1
    fi
  fi
  echo "== 导入 $(basename "$im_f") -> $REDIS_CONTAINER:$REDIS_DIR/$REDIS_RDB 并重启 =="
  docker cp "$im_f" "$REDIS_CONTAINER:$REDIS_DIR/$REDIS_RDB"
  docker restart "$REDIS_CONTAINER"
  echo "导入完成（容器已重启，Redis 启动时加载 RDB）"
}

do_list() {
  ls -lh -- "$BACKUP_DIR" 2>/dev/null | grep -E '\.rdb$' || echo "（$BACKUP_DIR/ 里还没有 Redis 备份）"
}

bk_cmd="${1:-backup}"
case "$bk_cmd" in
  backup) do_backup ;;
  import) shift; [ -n "${1:-}" ] || usage; do_import "$1" ;;
  list)   do_list ;;
  *)      usage ;;
esac
