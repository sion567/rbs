#!/bin/sh
# backup-docker-volume.sh —— docker 数据卷备份 / 恢复（同一套约定）
# ---------------------------------------------------------------------------
# 用法：
#   sh backup-docker-volume.sh backup       把 VOLUMES 里所有卷打包成压缩 tar
#   sh backup-docker-volume.sh import <文件> 清空并恢复卷（危险：需确认）
#   sh backup-docker-volume.sh list         列出已有备份
# 约定：
#   - 备份文件：docker-backups/vol_<卷名>.YYYYMMDD-HHMMSS.tar.gz
#   - 每卷默认保留最近 KEEP_BACKUPS=10 个，超出自动清理最老的
#   - 打包用临时 alpine 容器挂载卷（WORK_IMAGE 可换成本地已有的小镜像）
#   - 完整性校验：gzip 完好 + tar 能列出内容
#   - 导入 = 清空卷内容再解包（破坏性，交互确认 / FORCE=1）
# 场景：
#   - 数据库之外的有状态数据（上传文件、卷挂载的配置等）随发布一起备份
#   - 卷数据迁移到新机器
# 集成：多库/多对象场景参考 README「多个数据库」
# 纯 POSIX sh。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可环境变量覆盖）=====
VOLUMES="${VOLUMES:-appdata}"                 # 要备份的卷名，空格分隔多个
WORK_IMAGE="${WORK_IMAGE:-alpine:3.19}"       # 打包用的临时镜像（本地需已存在）
BACKUP_DIR="${BACKUP_DIR:-docker-backups}"
KEEP_BACKUPS="${KEEP_BACKUPS:-10}"
FORCE="${FORCE:-0}"
# ===========================================

usage() {
  echo "用法：sh backup-docker-volume.sh [backup | import <文件> | list]" >&2
  exit 1
}

verify_tar() { # $1=文件（注意：GNU tar 不支持 -- 结束符，别加）
  gzip -t -- "$1" 2>/dev/null || return 1
  [ -n "$(tar -tzf "$1" 2>/dev/null | head -1)" ]
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
  bk_abs="$(cd "$BACKUP_DIR" && pwd)"     # -v 挂载需要绝对路径
  bk_fail=0
  for vol in $VOLUMES; do
    bk_f="$BACKUP_DIR/vol_$vol.$(date +%Y%m%d-%H%M%S).tar.gz"
    echo "== 备份卷 $vol -> $bk_f =="
    if ! docker run --rm -v "$vol":/src:ro -v "$bk_abs":/dst "$WORK_IMAGE" \
        tar czf "/dst/$(basename "$bk_f")" -C /src .; then
      echo "backup-docker-volume.sh：$vol 打包失败。排查：卷是否存在 / $WORK_IMAGE 是否可拉起" >&2
      bk_fail=1
      continue
    fi
    if verify_tar "$bk_f"; then
      echo "完成（$(du -h "$bk_f" | cut -f1)）"
      echo "BACKED_UP: $bk_f"
      prune_files "vol_$vol" ".tar.gz"
    else
      rm -f -- "$bk_f"
      echo "backup-docker-volume.sh：$vol 备份不完整，已删除" >&2
      bk_fail=1
    fi
  done
  [ "$bk_fail" -eq 0 ] || exit 1
  echo "数据卷备份完成：$BACKUP_DIR/"
}

do_import() { # $1=备份文件
  im_f="$1"
  [ -f "$im_f" ] || { echo "backup-docker-volume.sh：找不到备份文件 $im_f" >&2; exit 1; }
  verify_tar "$im_f" || { echo "backup-docker-volume.sh：不是有效的卷备份，拒绝导入：$im_f" >&2; exit 1; }
  im_base="$(basename "$im_f")"; im_base="${im_base%.tar.gz}"
  im_vol="${im_base%.*}"          # 去时间戳
  im_vol="${im_vol#vol_}"         # 去前缀
  case "$im_vol" in
    ''|*.*) echo "backup-docker-volume.sh：无法从文件名解析卷名（应为 vol_<卷名>.YYYYMMDD-HHMMSS.tar.gz）：$im_f" >&2; exit 1 ;;
  esac
  if [ "$FORCE" != "1" ]; then
    if [ -t 0 ]; then
      printf '将清空卷 %s 并用 %s 恢复（现有数据丢失！）。确认请输入 yes：' "$im_vol" "$(basename "$im_f")"
      read -r im_reply
      [ "$im_reply" = "yes" ] || { echo "已取消"; exit 1; }
    else
      echo "backup-docker-volume.sh：非交互终端不执行导入。确认无误后加 FORCE=1：FORCE=1 sh backup-docker-volume.sh import $im_f" >&2
      exit 1
    fi
  fi
  im_abs="$(cd "$(dirname "$im_f")" && pwd)"
  echo "== 恢复 $(basename "$im_f") -> 卷 $im_vol（先清空）=="
  docker run --rm -v "$im_vol":/dst -v "$im_abs":/src:ro "$WORK_IMAGE" \
    sh -c "cd /dst && rm -rf ./* ./.[!.]* 2>/dev/null; tar xzf /src/$(basename "$im_f") -C /dst"
  echo "恢复完成：卷 $im_vol"
}

do_list() {
  ls -lh -- "$BACKUP_DIR" 2>/dev/null | grep -E '\.tar\.gz$' || echo "（$BACKUP_DIR/ 里还没有数据卷备份）"
}

bk_cmd="${1:-backup}"
case "$bk_cmd" in
  backup) do_backup ;;
  import) shift; [ -n "${1:-}" ] || usage; do_import "$1" ;;
  list)   do_list ;;
  *)      usage ;;
esac
