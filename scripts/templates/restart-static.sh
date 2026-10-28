#!/bin/sh
# restart.sh —— 静态前端包部署（由 release.sh 在包发布完后自动调用）
# ---------------------------------------------------------------------------
# 调用方式：① release.sh 自动调用：cd 到目标目录后执行 sh restart.sh <包名>...
#           ② 手动重发（不发新包、不带参数）：sh targets/<目标名>/restart.sh
# 返回值：  解压失败 / 解压后没有 index.html -> 退出码非 0；
#           release.sh 据此自动把包回滚到上一个安全版本，并重新执行本脚本
# 语法：    纯 POSIX sh
# 适用：    webpack/vite 打出的 dist.tar.gz、uniapp H5 包等静态站点。
#           注意"回滚"的实现方式：release.sh 恢复旧 tar.gz 后重跑本脚本重新
#           解压，index.html 等同名文件即恢复旧版；带 hash 的旧静态资源会
#           残留（无害，确认稳定后清 WEB_ROOT.bak-* 即可）。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可被环境变量覆盖，如 WEB_ROOT=/data/site sh restart.sh dist.tar.gz）=====
WEB_ROOT="${WEB_ROOT:-/usr/share/nginx/html}"   # 静态文件根目录（nginx server 的 root）
STRIP="${STRIP:-0}"                             # 包里若带一层目录（如 dist/...），改成 1（解压时剥掉）
KEEP_OLD="${KEEP_OLD:-1}"                       # 解压前把现有 WEB_ROOT 备份为 WEB_ROOT.bak-时间戳
# ========================

APP_DIR="$(cd "$(dirname "$0")" && pwd)"

# 取包：优先用 release.sh 传入的第一个 .tar.gz 参数（相对目标目录）；没传则用目录里的
PKG=""
for _a in "$@"; do
  case "$_a" in *.tar.gz|*.tgz) PKG="$_a"; break ;; esac
done
if [ -n "$PKG" ]; then
  case "$PKG" in
    /*) PKG_PATH="$PKG" ;;
    *)  PKG_PATH="$APP_DIR/$PKG" ;;
  esac
else
  set -- "$APP_DIR"/*.tar.gz
  if [ -f "$1" ]; then PKG_PATH="$1"; fi
fi
if [ -z "${PKG_PATH:-}" ] || [ ! -f "$PKG_PATH" ]; then
  echo "restart.sh：$APP_DIR 里找不到 dist.tar.gz（参数 $*）" >&2
  exit 1
fi

echo "== [1/2] 部署静态文件（$(basename "$PKG_PATH") -> $WEB_ROOT）=="
mkdir -p "$WEB_ROOT"
if [ "$KEEP_OLD" = "1" ] && [ -f "$WEB_ROOT/index.html" ]; then
  _bak="$WEB_ROOT.bak-$(date +%Y%m%d%H%M%S)"
  cp -a "$WEB_ROOT" "$_bak"
  echo "已备份现有站点：$_bak"
fi
if [ "$STRIP" = "1" ]; then
  tar -xzf "$PKG_PATH" -C "$WEB_ROOT" --strip-components=1
else
  tar -xzf "$PKG_PATH" -C "$WEB_ROOT"
fi

echo "== [2/2] 检查部署结果 =="
if [ ! -f "$WEB_ROOT/index.html" ]; then
  echo "restart.sh：解压后在 $WEB_ROOT 找不到 index.html" >&2
  echo "  若包里带了一层目录（如 dist/），把脚本顶部 STRIP 改成 1 再试" >&2
  exit 1
fi
# 纯静态文件即时生效，无需重启 nginx；若有 nginx 配置变更需求，放开下一行注释：
# nginx -t && nginx -s reload
echo "部署完成：$WEB_ROOT（$(date '+%F %T')）"
