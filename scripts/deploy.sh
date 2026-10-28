#!/bin/sh
# deploy.sh —— 【发布】入口（内部调用同目录 release.sh，参数原样透传）
# 加班发版专用：不用记参数。
#   sh deploy.sh                正式发布 sources/ 里的包
#   sh deploy.sh --dry-run      演练（先看一眼再发）
# 其他选项见 release.sh --help。
exec bash "$(dirname "$0")/release.sh" "$@"
