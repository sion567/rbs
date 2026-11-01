#!/bin/sh
# backup-docker-image.sh —— docker 镜像导出 / 导入（docker save / load，同一套约定）
# ---------------------------------------------------------------------------
# 用法：
#   sh backup-docker-image.sh backup       把 IMAGES 里所有镜像导出为压缩 tar
#   sh backup-docker-image.sh import <文件> docker load 导入（叠加式，无破坏）
#   sh backup-docker-image.sh list         列出已有备份
# 约定：
#   - 备份文件：docker-backups/image_<净化镜像名>.YYYYMMDD-HHMMSS.tar.gz
#     （名字里的 / : 会替换成 _，如 myapp:1.0 -> image_myapp_1.0）
#   - 每镜像默认保留最近 KEEP_BACKUPS=10 个，超出自动清理最老的
#   - 完整性校验：gzip 完好 + tar 里含 manifest.json（docker save 的标志文件）
#   - 导入是叠加式的（docker load 不影响现有容器），所以不需要 FORCE 确认
# 场景：
#   - 发布前把新版镜像存档；服务器迁移离线搬运镜像；公网拉不到镜像的内网环境
# 集成：多库/多对象场景参考 README「多个数据库」
# 纯 POSIX sh。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可环境变量覆盖）=====
IMAGES="${IMAGES:-myapp:latest}"              # 要备份的镜像，空格分隔多个（含 tag 必填）
BACKUP_DIR="${BACKUP_DIR:-docker-backups}"
KEEP_BACKUPS="${KEEP_BACKUPS:-10}"
# ===========================================

usage() {
  echo "用法：sh backup-docker-image.sh [backup | import <文件> | list]" >&2
  exit 1
}

# 镜像名 -> 文件名片段：/ 和 : 替换成 _
sanitize() {
  printf '%s' "$1" | tr '/:' '__'
}

verify_image_tar() { # $1=文件（注意：GNU tar 不支持 -- 结束符，别加）
  gzip -t -- "$1" 2>/dev/null || return 1
  tar -tzf "$1" 2>/dev/null | grep -q '^manifest\.json$'
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
  for img in $IMAGES; do
    bk_f="$BACKUP_DIR/image_$(sanitize "$img").$(date +%Y%m%d-%H%M%S).tar.gz"
    echo "== 导出镜像 $img -> $bk_f =="
    docker save "$img" | gzip > "$bk_f"
    if verify_image_tar "$bk_f"; then
      echo "完成（$(du -h "$bk_f" | cut -f1)）"
      echo "BACKED_UP: $bk_f"
      prune_files "image_$(sanitize "$img")" ".tar.gz"
    else
      rm -f -- "$bk_f"
      echo "backup-docker-image.sh：$img 导出不完整（tar 里没有 manifest.json），已删除。排查：镜像名/tag 是否存在" >&2
      bk_fail=1
    fi
  done
  [ "$bk_fail" -eq 0 ] || exit 1
  echo "镜像备份完成：$BACKUP_DIR/"
}

do_import() { # $1=备份文件
  im_f="$1"
  [ -f "$im_f" ] || { echo "backup-docker-image.sh：找不到备份文件 $im_f" >&2; exit 1; }
  verify_image_tar "$im_f" || { echo "backup-docker-image.sh：不是有效的镜像导出文件，拒绝导入：$im_f" >&2; exit 1; }
  echo "== docker load < $(basename "$im_f") =="
  docker load -i "$im_f"
  echo "导入完成（叠加式，未动任何运行中的容器）"
}

do_list() {
  ls -lh -- "$BACKUP_DIR" 2>/dev/null | grep -E '\.tar\.gz$' || echo "（$BACKUP_DIR/ 里还没有镜像备份）"
}

bk_cmd="${1:-backup}"
case "$bk_cmd" in
  backup) do_backup ;;
  import) shift; [ -n "${1:-}" ] || usage; do_import "$1" ;;
  list)   do_list ;;
  *)      usage ;;
esac
