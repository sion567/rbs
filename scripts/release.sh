#!/usr/bin/env bash
# 必须用 bash 运行（麒麟桌面版等 Debian 系的 sh 是 dash，缺关联数组语法）
if [ -z "${BASH_VERSION:-}" ]; then
  echo "错误：请用 bash 运行本脚本：bash release.sh（不要用 sh release.sh）" >&2
  exit 1
fi
# 关联数组、mapfile 需要 bash 4+（麒麟 V10 / CentOS / Ubuntu 主流版本都满足）
if [ "${BASH_VERSINFO[0]}" -lt 4 ]; then
  echo "错误：bash 版本过低（当前 $BASH_VERSION，需要 4.0 及以上）" >&2
  exit 1
fi
# 依赖自检：缺了就在开头人话报错，而不是跑到一半莫名失败（flock/timeout 可选，不查）
for _cmd in sha256sum df awk sort tee; do
  command -v "$_cmd" >/dev/null 2>&1 || { echo "错误：系统缺少命令 $_cmd，请先安装（一般属于 coreutils/gawk 基础包）" >&2; exit 1; }
done
# ============================================================================
# release.sh —— rbs 正式版发布脚本（在服务器上、sources/ 与 targets/ 同级目录运行）
#
# 目录约定：
#   sources/    你上传的待发布包。三种用法：
#              ① 平铺 sources/xxx.jar：发给所有"目录里已有同名旧包"的目标，
#                 适合全局唯一的包名（如后端 jar，一次发给 N 台）。
#              ② 子目录 sources/<目标名>/xxx：只发给该目标。给目标首次上包、
#                 或多个目标用同一个包名（手机端/web 端都叫 dist.tar.gz）
#                 时必须用这种。
#              ③ 【推荐】清单 manifest.txt（放脚本旁边），明确写每个包
#                 发到哪，详见下方「发布清单」一节。
#              同一个包名同时出现在平铺和子目录、且两者会发到同一个
#              目标时，脚本报错拒绝（无法判断该用哪个文件）。
#   targets/   每个子目录是一个发布目标，放着当前版本的包 + 发布脚本 sh。
#              新目标的目录（含发布 sh）要先手工建好，脚本不会替你创建。
#              发布脚本按以下顺序确定：
#              ① targets/<目标名>/.deploy-script（第 1 行脚本名；可选
#                 第 2 行 no-args = 执行时不传包名参数）
#              ② 环境变量 DEPLOY_SH=xxx.sh（所有目标统一改名）
#              ③ restart.sh（默认名，用 sh restart.sh 方式执行）
#              ④ 目录里唯一的一个 *.sh（自动识别，叫什么名字都行）
#
# 发布清单 manifest.txt（推荐，放脚本旁边，与 sources/、targets/ 同级）：
#   它是"路由表"式的稳定配置：一次把所有包写全，平时发布不用动它——
#   只上传这次改过的包到 sources/，清单里没上传的包自动跳过。
#   只有新增包、新目标或调整分发关系时，才需要加/改一行。
#   格式：每行一条「文件路径 目标名 [目标名2 ...]」
#         - 路径相对 sources/（子目录里的包写 web/dist.tar.gz 这种相对路径）
#         - 目标名必须是 targets/ 下真实存在的目录，一行可写多个
#         - 空格 / Tab 分隔均可；# 开头为注释行；空行忽略；CRLF 也能读
#   示例（manifest.txt 内容）：
#         # 后端 jar，首发也直接平铺放
#         xx-system-server.jar     xx-system
#         # 手机端、web 端同名包，各发各的
#         web/dist.tar.gz          web
#         mobile/dist.tar.gz       mobile
#         # 同一个包一次发多台
#         agent.jar                app1 app2
#   规则：
#   - 只发 sources/ 里实际存在的包：清单是超集，这次没上传的不发。
#   - sources/ 里出现清单没有的包 -> 报错并提示该加的一行（防漏发）。
#   - 目标名拼错、路径写错、重复列同一个文件、清单为空 -> 直接报错。
#   - 删掉清单 -> 回退到上面的目录约定规则。
#
# 每个目标的发布流程（每一步都有校验）：
#   0. 备库   有 backup-db.sh 钩子时，发布前先做数据库原始快照（带 SQL 变更的
#             发版的回滚依据）；快照失败直接中止发布（DB_HOOK_SOFT=1 可硬发）
#   1. 快照   把当前线上包原样存成带时间戳的还原点 xxx.YYYYMMDD-HHMMSS
#             - 每次发布都追加一个还原点（不覆盖）：熬夜连发 10 版就有
#               10 个还原点，跨天也不会把调试版"占"成安全版
#             - 每包默认保留最近 KEEP_BACKUPS=10 个，发布成功后自动清理更老的
#             - 旧包不存在（首次发布）-> 无还原点
#             - 快照后做 sha256 校验，与原文件不一致则删除并报错
#   2. 替换   source 包先拷成目标目录里的隐藏临时文件，sha256 校验一致后
#             原子改名顶上；顶上后再次 sha256 校验
#   3. 执行   cd 到目标目录执行发布 sh（默认把包名作为参数传入；
#             .deploy-script 第 2 行写 no-args 则不传参数）
#             - sh 失败 -> 自动用"本次发布前"的还原点恢复旧包（逐包
#               sha256 校验），重跑一次 sh；重跑仍失败则报错终止
#
# 其他安全项：开始前校验 source 非空、每个包都能对上目标、磁盘剩余空间；
# flock 防并发；全程写日志到 logs/；--dry-run 演练不改任何文件。
#
# 手动备份 / 还原（也有等价的独立入口 deploy.sh / backup.sh / restore.sh）：
#   bash release.sh --backup      手动备份：快照各目标线上包 + 数据库钩子 backup-db.sh
#   bash release.sh --restore     交互式列出还原点，选一个恢复并重跑 sh
#   bash release.sh --restore 20261027-215844   直接恢复指定还原点
#   数据库导入（危险，手动做）：sh backup-db.sh import db-backups/xxx.sql.gz
#
# 用法：
#   bash release.sh                  正式发布（targets/ 下所有目标）
#   bash release.sh --targets a b    只发布指定目标
#   bash release.sh --dry-run        演练：只打印计划
#   bash release.sh --keep-going     某目标失败后继续其余目标（默认立即终止）
#   bash release.sh --backup         手动备份（快照）当前线上包
#   bash release.sh --restore [ts]   还原：无 ts 交互选择，有 ts 直接恢复
#
# 可用环境变量覆盖：SOURCE_DIR  TARGETS_DIR  DEPLOY_SH  KEEP_BACKUPS=10
#                   LOG_KEEP=30（日志保留个数）  SH_TIMEOUT=600（发布 sh 超时秒数，0=不限）
#                   MIN_FREE_MB=200  DB_HOOK_SOFT=0（=1 时数据库快照失败仍继续发布）
#
# 依赖：bash 4+（已兼容 4.0~4.3 的空数组限制，CentOS 7 / 部分国产系统的 4.2 也能跑）、
#       coreutils（sha256sum/cp/mv/df）；flock、timeout 可选。
# 约定：目录名 / 包名不要带空格。首次放到服务器后：chmod +x release.sh
# ============================================================================

set -uo pipefail

cd "$(dirname "$0")" || { echo "无法进入脚本所在目录" >&2; exit 1; }

SOURCE_DIR="${SOURCE_DIR:-sources}"
MANIFEST_NAME="${MANIFEST_NAME:-manifest.txt}"   # 发布清单文件名（放脚本旁边，与 sources/ targets/ 同级）
TARGETS_DIR="${TARGETS_DIR:-targets}"
DEPLOY_SH="${DEPLOY_SH:-restart.sh}"
SH_TIMEOUT="${SH_TIMEOUT:-600}"
MIN_FREE_MB="${MIN_FREE_MB:-200}"
KEEP_BACKUPS="${KEEP_BACKUPS:-10}"   # 每个包保留的还原点数量（超出自动清理最老的）
DB_HOOK="${DB_HOOK:-backup-db.sh}"   # --backup 时顺带执行的数据库备份钩子（deploy 根目录，可选）
LOG_DIR="${LOG_DIR:-logs}"

# ---------------- 颜色与日志 ----------------
if [[ -t 1 ]]; then
  C_G=$'\e[32m'; C_Y=$'\e[33m'; C_R=$'\e[31m'; C_B=$'\e[36m'; C_0=$'\e[0m'
else
  C_G='' C_Y='' C_R='' C_B='' C_0=''
fi
now()  { date '+%H:%M:%S'; }
info() { printf '[%s] %sINFO%s %s\n' "$(now)" "$C_B" "$C_0" "$*"; }
ok()   { printf '[%s] %sOK  %s %s\n'    "$(now)" "$C_G" "$C_0" "$*"; }
warn() { printf '[%s] %sWARN%s %s\n' "$(now)" "$C_Y" "$C_0" "$*"; }
err()  { printf '[%s] %sERR %s %s\n'  "$(now)" "$C_R" "$C_0" "$*" >&2; }
die()  { err "$*"; exit 1; }

usage() {
  cat <<'EOF'
用法：bash release.sh [选项]
  （默认）          发布：sources/ 里的包 -> targets/ 各目标 -> 执行发布 sh
  --dry-run         演练：只打印将要做什么，不改动任何文件、不执行 sh
  --targets A B     只处理 targets/ 下指定的目标
  --keep-going      某个目标失败后继续处理其余目标（默认立即终止）
  --backup          手动备份：快照各目标线上包 + 执行数据库钩子 backup-db.sh（如有）
  --restore [ts]    还原：列出还原点选一个恢复并重跑 sh；带 ts 直接恢复
还原点是追加式的（包名.时间戳，每包默认留最近 10 个），跨天连发不会互相覆盖
三个等价入口：sh deploy.sh（发布）/ sh backup.sh（备份）/ sh restore.sh（还原）
【推荐】发布清单 manifest.txt（放脚本旁边，与 sources/ targets/ 同级）：每行「文件路径 目标名...」
  路由表式稳定配置：一次写全、平时发布不用改；只传这次改动的包，清单里没传的包自动跳过
  例：xx-system-server.jar  xx-system      首发（无同名旧包）也直接发
      web/dist.tar.gz       web            手机端/web 端同名包，各发各的
      agent.jar             app1 app2      一个包一次发多台
  sources/ 出现清单没有的包会报错并提示该加的一行；删掉清单回退目录约定
无清单时的目录约定：平铺=发给有同名旧包的目标；子目录 sources/<目标名>/=只发该目标
首次发布：targets/<目标名>/（含发布 sh）要先手工建好
发布脚本：.deploy-script（第1行脚本名，可选第2行 no-args=不传包名）> DEPLOY_SH > restart.sh > 目录里唯一 *.sh
          默认执行方式：sh 脚本名 包名...；脚本自己的 $1 有其他用途的加 no-args
环境变量：SOURCE_DIR / TARGETS_DIR / DEPLOY_SH / KEEP_BACKUPS / SH_TIMEOUT / MIN_FREE_MB / LOG_DIR
完整约定见脚本头部注释。
EOF
}

# ---------------- 参数解析 ----------------
MODE="deploy"; DRY_RUN=0; KEEP_GOING=0
DATE="$(date +%Y%m%d)"
RESTORE_TS=""
TARGET_FILTER=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)    DRY_RUN=1 ;;
    --backup)     MODE="backup" ;;
    --restore)    MODE="restore"
                  if [[ $# -ge 2 && "$2" != -* ]]; then RESTORE_TS="$2"; shift; fi ;;
    --keep-going) KEEP_GOING=1 ;;
    --targets)
      shift
      while [[ $# -gt 0 && "$1" != -* ]]; do TARGET_FILTER+=("$1"); shift; done
      [[ "${TARGET_FILTER[@]+"${#TARGET_FILTER[@]}"}" -gt 0 ]] || die "--targets 后面要跟至少一个目标名"
      continue ;;
    -h|--help) usage; exit 0 ;;
    *) die "未知参数：$1（--help 看用法）" ;;
  esac
  shift
done
[[ -z "$RESTORE_TS" || "$RESTORE_TS" =~ ^[0-9]{8}-[0-9]{6}$ ]] || die "还原点时间戳格式不对：$RESTORE_TS（应为 YYYYMMDD-HHMMSS）"

# ---------------- 全局预检 ----------------
[[ -d "$SOURCE_DIR"  ]] || die "找不到 ./$SOURCE_DIR/ 目录（应与本脚本同级）"
[[ -d "$TARGETS_DIR" ]] || die "找不到 ./$TARGETS_DIR/ 目录（应与本脚本同级）"
[[ $EUID -eq 0 ]] && warn "当前是 root 在跑，确认这是你的发布账号？"

shopt -s nullglob

TARGETS_ALL=()
for d in "$TARGETS_DIR"/*/; do
  TARGETS_ALL+=("$(basename "$d")")
done
[[ "${TARGETS_ALL[@]+"${#TARGETS_ALL[@]}"}" -gt 0 ]] || die "$TARGETS_DIR/ 下没有任何目标子目录"

if [[ "${TARGET_FILTER[@]+"${#TARGET_FILTER[@]}"}" -gt 0 ]]; then
  for t in "${TARGET_FILTER[@]}"; do
    [[ -d "$TARGETS_DIR/$t" ]] || die "--targets 指定的目标不存在：$t"
  done
  TARGETS_TO_DO=("${TARGET_FILTER[@]}")
else
  TARGETS_TO_DO=("${TARGETS_ALL[@]}")
fi
mapfile -t TARGETS_TO_DO < <(printf '%s\n' "${TARGETS_TO_DO[@]}" | sort -u)

# 磁盘剩余空间（回滚/发布都要写文件）
free_mb="$(df -Pm "$TARGETS_DIR" 2>/dev/null | awk 'NR==2{print $4}')"
if [[ "$free_mb" =~ ^[0-9]+$ ]] && (( free_mb < MIN_FREE_MB )); then
  die "磁盘空间不足：剩余 ${free_mb}MB < 要求 ${MIN_FREE_MB}MB"
fi

# ---------------- 通用小工具 ----------------
sha256_of() { sha256sum -- "$1" | awk '{print $1}'; }

TMPFILES=()
cleanup() { local f; for f in ${TMPFILES[@]+"${TMPFILES[@]}"}; do rm -f -- "$f"; done; }
trap cleanup EXIT

# src -> dir/.名字.reltmp.$$ -> sha256 校验 -> 原子 mv 成 dir/final -> 再校验
copy_verified() { # $1=src $2=dir $3=final_name
  local src="$1" dir="$2" final="$3" tmp="$2/.${3}.reltmp.$$" want got
  TMPFILES+=("$tmp")
  cp -p -- "$src" "$tmp" || { err "拷贝失败：$src -> $tmp"; return 2; }
  want="$(sha256_of "$src")"; got="$(sha256_of "$tmp")"
  [[ "$want" == "$got" ]] || { err "拷贝后 sha256 不一致：$tmp"; return 2; }
  mv -f -- "$tmp" "$dir/$final" || { err "原子改名失败：$dir/$final"; return 2; }
  got="$(sha256_of "$dir/$final")"
  [[ "$want" == "$got" ]] || { err "发布后 sha256 不一致：$dir/$final"; return 2; }
  return 0
}

# ---- 还原点（快照）三件套 ----
# 命名：包名.YYYYMMDD-HHMMSS，追加式，每次发布/手动备份各产生一个，
# 不互相覆盖——跨天熬夜连发也不会把调试版"占"成安全版。
is_snap_name() { # $1=文件名  是否为快照命名（包名部分任意，尾部 .时间戳）
  local base="$1" ts
  case "$base" in *.*) ;; *) return 1 ;; esac
  ts="${base##*.}"
  [[ "$ts" =~ ^[0-9]{8}-[0-9]{6}$ ]]
}

# 把当前线上包快照成还原点。返回 0=已快照 1=无现包（首次发布）2=失败
# 结果写入全局 SNAP_NAME
snapshot_pkg() { # $1=tdir $2=pkg
  local tdir="$1" pkg="$2" cur="$1/$2" want got
  SNAP_NAME=""
  [[ -f "$cur" ]] || return 1
  SNAP_NAME="$pkg.$(date +%Y%m%d-%H%M%S)"
  while [[ -e "$tdir/$SNAP_NAME" ]]; do   # 同一秒已有同名还原点，等 1 秒重取
    sleep 1
    SNAP_NAME="$pkg.$(date +%Y%m%d-%H%M%S)"
  done
  want="$(sha256_of "$cur")" || { err "无法读取待快照文件：$cur"; return 2; }
  cp -p -- "$cur" "$tdir/$SNAP_NAME" || { err "快照拷贝失败：$cur"; return 2; }
  got="$(sha256_of "$tdir/$SNAP_NAME")"
  [[ "$want" == "$got" ]] || { err "快照 sha256 不一致，已删除：$tdir/$SNAP_NAME"; rm -f -- "$tdir/$SNAP_NAME"; return 2; }
  return 0
}

# 清理该包超出 KEEP_BACKUPS 的最老还原点（发布/备份成功后调用）
prune_snapshots() { # $1=tdir $2=pkg
  local tdir="$1" pkg="$2" keep="${KEEP_BACKUPS:-10}" f base
  [[ "$keep" =~ ^[0-9]+$ && "$keep" -gt 0 ]] || return 0
  local -a snaps=()
  for f in "$tdir/$pkg".*; do
    [[ -f "$f" ]] || continue
    base="$(basename "$f")"
    is_snap_name "$base" || continue
    snaps+=("$base")
  done
  [[ "${snaps[@]+"${#snaps[@]}"}" -gt "$keep" ]] || return 0
  mapfile -t snaps < <(printf '%s\n' ${snaps[@]+"${snaps[@]}"} | sort)
  local n="${#snaps[@]}" i
  for ((i = 0; i < n - keep; i++)); do
    rm -f -- "$tdir/${snaps[$i]}"
    info "清理最老还原点：${snaps[$i]}"
  done
}

# 列出目标目录里所有还原点，输出「时间戳 包名」行（时间戳无序）
list_snapshots() { # $1=tdir
  local tdir="$1" f base
  for f in "$tdir"/*; do
    [[ -f "$f" ]] || continue
    base="$(basename "$f")"
    is_snap_name "$base" || continue
    echo "${base##*.} ${base%.*}"
  done
}

# 找发布脚本，优先级：
#   ① targets/<t>/.deploy-script：
#        第 1 行 = 脚本名
#        第 2 行 = 可选 no-args（执行时不传包名参数；适合脚本自己的 $1 有其他用途的）
#   ② 环境变量 DEPLOY_SH（全局统一改名，仍默认传包名）
#   ③ restart.sh（默认名，用 sh restart.sh 方式执行）
#   ④ 目录里唯一的 *.sh（自动识别，叫什么都行）
# 结果写入全局 SH_NAME / SH_NOARGS（1 = 不传包名），返回 0；找不到返回 1
resolve_sh() { # $1=tdir
  SH_NAME=""; SH_NOARGS=0
  local tdir="$1" cand=() name line2=""
  if [[ -f "$tdir/.deploy-script" ]]; then
    { IFS= read -r name; IFS= read -r line2; } < "$tdir/.deploy-script" || true
    name="${name%$'\r'}"; name="${name#"${name%%[![:space:]]*}"}"; name="${name%"${name##*[![:space:]]}"}"
    line2="${line2%$'\r'}"; line2="${line2#"${line2%%[![:space:]]*}"}"; line2="${line2%"${line2##*[![:space:]]}"}"
    [[ -n "$name" ]] || die "$tdir/.deploy-script 第 1 行应写脚本名"
    [[ -f "$tdir/$name" ]] || die "$tdir/.deploy-script 指向的 '$name' 不存在（写相对该目标目录的文件名）"
    case "$line2" in
      "") ;;
      no-args|noargs) SH_NOARGS=1 ;;
      *) die "$tdir/.deploy-script 第 2 行只支持 no-args（执行时不传包名），收到的是 '$line2'" ;;
    esac
    SH_NAME="$name"; return 0
  fi
  if [[ -f "$tdir/$DEPLOY_SH" ]]; then SH_NAME="$DEPLOY_SH"; return 0; fi
  cand=( "$tdir"/*.sh )
  if [[ "${cand[@]+"${#cand[@]}"}" -eq 1 ]]; then SH_NAME="$(basename "${cand[0]}")"; return 0; fi
  return 1
}

run_deploy_sh() { # $1=tdir $2=脚本名 $3=SH_NOARGS(1=不传包名) 其余=包名...
  local tdir="$1" sh_rel="$2" noargs="$3"; shift 3
  local -a cmd=(sh "./$sh_rel")
  [[ "$noargs" == "1" ]] || cmd+=("$@")
  (
    cd "$tdir" || exit 97
    if [[ "$SH_TIMEOUT" != "0" ]] && command -v timeout >/dev/null 2>&1; then
      exec timeout -k 10 "$SH_TIMEOUT" "${cmd[@]}"
    else
      exec "${cmd[@]}"
    fi
  )
}

acquire_lock() {
  command -v flock >/dev/null 2>&1 || { warn "系统没有 flock，跳过并发锁（请勿同时跑两个发布）"; return 0; }
  exec 9>>"$TARGETS_DIR/.release.lock" || die "无法创建锁文件"
  flock -n 9 || die "已有另一个发布正在运行（$TARGETS_DIR/.release.lock）"
}

setup_log() {
  mkdir -p -- "$LOG_DIR" || die "无法创建日志目录 $LOG_DIR/"
  LOG_FILE="$LOG_DIR/${MODE}-$(date +%Y%m%d-%H%M%S).log"
  exec > >(tee -a -- "$LOG_FILE") 2>&1
  info "日志：$LOG_FILE"
  # 旧日志按修改时间只保留最近 LOG_KEEP 个，防无限增长
  local keep="${LOG_KEEP:-30}" old
  if [[ "$keep" =~ ^[0-9]+$ && "$keep" -gt 0 ]]; then
    while IFS= read -r old; do rm -f -- "$old"; done < <(ls -t "$LOG_DIR"/*.log 2>/dev/null | tail -n +"$((keep + 1))")
  fi
}

print_summary() { # $@=结果行
  echo
  info "================ 结果汇总 ================"
  if [[ $# -gt 0 ]]; then printf '%s\n' "$@"; else info "（无）"; fi
}

# ---------------- source -> targets 映射（仅发布模式） ----------------
declare -A PLAN   # "目标|包名" -> source 文件路径

MANIFEST="$MANIFEST_NAME"   # 清单是稳定配置，放脚本旁边；sources/ 是上传区，可以随时清空

# 清单模式：manifest.txt（脚本旁边）是路由表，一次写全所有包。
# 清单里这次没上传的包自动跳过；sources/ 里出现清单没有的包则报错提示补行。
# 每行「文件路径 目标名 [目标名2 ...]」（路径相对 sources/，# 为注释）。
# 指到哪发到哪，不要求目标里有同名旧包（首发也能平铺）；
# sources/ 里所有包都必须列出，防止漏发。
build_plan_from_manifest() {
  local line path rest t rel d
  local -A seen=() files=()

  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"                      # 容忍 Windows 编辑出来的 CRLF
    [[ "$line" =~ ^[[:space:]]*$ ]] && continue
    [[ "$line" == \#* ]] && continue
    read -r path rest <<< "$line"
    [[ -n "$path" && -n "$rest" ]] || die "$MANIFEST 格式错误，每行应为：文件路径 目标名 [目标名2 ...] —— 问题行：$line"
    path="${path#./}"
    [[ -z "${seen[$path]:-}" ]] || die "$MANIFEST 里重复列了同一个文件：sources/$path"
    seen["$path"]=1
    if [[ ! -f "$SOURCE_DIR/$path" ]]; then
      info "$MANIFEST 里的 sources/$path 这次没上传，跳过"
      continue
    fi
    for t in $rest; do
      [[ " ${TARGETS_ALL[*]} " == *" $t "* ]] || die "$MANIFEST 里的目标不存在：'$t'（targets/ 下没有这个目录，是不是拼错了？）"
      key="$t|$(basename "$path")"
      if [[ -n "${PLAN[$key]:-}" && "${PLAN[$key]}" != "$SOURCE_DIR/$path" ]]; then
        die "$MANIFEST 冲突：包 '$(basename "$path")' 发到目标 '$t' 有两个不同来源（${PLAN[$key]} 和 sources/$path），请删掉其中一行"
      fi
      PLAN["$key"]="$SOURCE_DIR/$path"
    done
  done < "$MANIFEST"

  [[ "${seen[@]+"${#seen[@]}"}" -gt 0 ]] || die "$MANIFEST 是空的。要么把清单写好，要么删掉它改用目录约定"

  # 枚举 sources/ 里实际存在的包（平铺 + 一级子目录，manifest.txt 本身除外）
  for f in "$SOURCE_DIR"/*; do
    [[ -f "$f" ]] || continue
    [[ "$(basename "$f")" == "$MANIFEST_NAME" ]] && continue
    files["$(basename "$f")"]=1
  done
  for sub in "$SOURCE_DIR"/*/; do
    d="${sub#"$SOURCE_DIR"/}"                 # 如 xx-system/
    for f in "$sub"*; do
      [[ -f "$f" ]] || continue
      [[ "$(basename "$f")" == "$MANIFEST_NAME" ]] && continue
      files["$d$(basename "$f")"]=1
    done
  done
  for rel in ${files[@]+"${!files[@]}"}; do
    [[ -n "${seen[$rel]:-}" ]] || die "sources/$rel 不在 $MANIFEST 里。请加一行：$rel  目标名1 [目标名2 ...]（清单是一次写全的稳定配置，只有新增包/目标时才需要加）"
  done
}

# 目录约定模式（无清单）：子目录按目标名、平铺按同名旧包猜
build_plan_by_name() {
  local sub t f pkg key found

  # ① 子目录形式 sources/<目标>/<包>：强制发到该目标。
  #    用于：给目标首次上包 / 多个目标用同名包（手机端、web 端都叫 dist.tar.gz）
  for sub in "$SOURCE_DIR"/*/; do
    t="$(basename "$sub")"
    [[ -d "$TARGETS_DIR/$t" ]] || die "$SOURCE_DIR/$t/ 对不上目标：targets/ 下没有 '$t' 这个目标（首发新目标请先在服务器上建好 targets/$t/ 并放入发布 sh，脚本不会自动创建）"
    for f in "$sub"*; do
      [[ -f "$f" ]] || continue
      [[ "$(basename "$f")" == "$MANIFEST_NAME" ]] && continue
      PLAN["$t|$(basename "$f")"]="$f"
    done
  done

  # ② 平铺形式 sources/<包>：发给所有"已存在同名旧包"的目标。
  #    若平铺包会发到某个已被子目录认领的目标（同名包），意图冲突，报错拒绝
  for f in "$SOURCE_DIR"/*; do
    [[ -f "$f" ]] || continue
    pkg="$(basename "$f")"
    [[ "$pkg" == "$MANIFEST_NAME" ]] && continue
    found=0
    for t in "${TARGETS_ALL[@]}"; do
      [[ -f "$TARGETS_DIR/$t/$pkg" ]] || continue
      if [[ -n "${PLAN[$t|$pkg]:-}" ]]; then
        die "包名 '$pkg' 冲突：$SOURCE_DIR/$pkg（平铺）与 $SOURCE_DIR/$t/$pkg（子目录）都会发到目标 '$t'，无法判断用哪个文件，请二选一（同名包发给不同目标请一律用子目录形式，或改用 $MANIFEST 明确指定）"
      fi
      PLAN["$t|$pkg"]="$f"
      found=1
    done
    [[ $found -eq 1 ]] || die "$SOURCE_DIR/$pkg 找不到对应目标（没有任何目标目录里有同名旧包）。给目标新增包请放到 $SOURCE_DIR/<目标名>/ 子目录里，或建一个 $MANIFEST 写明发到哪"
  done

  # ${arr[@]+"${#arr[@]}"} 写法：bash 4.2/4.3（CentOS 7 及部分国产系统）在 set -u 下
  # 会把空数组展开当成"未定义变量"，必须用这种防护；下同
  [[ "${PLAN[@]+"${#PLAN[@]}"}" -gt 0 ]] || die "$SOURCE_DIR/ 里没有任何可发布的文件"
}

build_plan() {
  if [[ -f "$MANIFEST" ]]; then
    [[ -f "$SOURCE_DIR/$MANIFEST_NAME" ]] && warn "sources/$MANIFEST_NAME 会被忽略（清单现在放脚本旁边，与 sources/ 同级），建议删掉它避免混淆"
    build_plan_from_manifest
  else
    build_plan_by_name
  fi

  [[ "${PLAN[@]+"${#PLAN[@]}"}" -gt 0 ]] || die "sources/ 里没有任何可发布的文件（或 $MANIFEST 里列的包这次都没上传）"

  # 源文件非空、可读（防上传一半的包）
  local key f
  for key in "${!PLAN[@]}"; do
    f="${PLAN[$key]}"
    [[ -s "$f" ]] || die "源文件是空的：$f（上传不完整？）"
    [[ -r "$f" ]] || die "源文件不可读：$f"
  done
}

# ---------------- 发布模式 ----------------
do_deploy() {
  build_plan

  if [[ $DRY_RUN -eq 1 ]]; then
    info "== 演练模式（不改动任何文件、不执行 sh），今天是 $DATE =="
  else
    acquire_lock
    setup_log
    info "== 正式发布开始，日期标记：$DATE =="
  fi

  local t tdir key pkg src sh_rel rc rrc t_failed
  local -a pkgs deployed results
  local -a DB_SNAP_HINTS=()
  FAILED=0

  # ---- 第 0 步：发布前数据库原始快照（带 SQL 变更的发版的回滚依据）----
  # 测试不通过时用它恢复数据（手动：sh backup-db.sh import <快照>），
  # 代码回滚走 --restore；两条腿都回才是完整的"回滚上一个版本"。
  if [[ -f "$DB_HOOK" && $DRY_RUN -eq 0 ]]; then
    info "== 发布前数据库原始快照（sh $DB_HOOK backup）=="
    local db_out="" db_rc=0 _l
    db_out="$(sh "$DB_HOOK" backup 2>&1)" && db_rc=0 || db_rc=$?
    [[ -n "$db_out" ]] && printf '%s\n' "$db_out"
    if [[ $db_rc -ne 0 ]]; then
      if [[ "${DB_HOOK_SOFT:-0}" == "1" ]]; then
        warn "数据库快照失败，但 DB_HOOK_SOFT=1，继续发布（注意：本次发布没有数据库回滚依据）"
      else
        die "数据库快照失败，已中止发布（各目标包尚未动）。确需硬发：DB_HOOK_SOFT=1 bash deploy.sh"
      fi
    fi
    while IFS= read -r _l; do
      [[ "$_l" == "BACKED_UP:"* ]] && DB_SNAP_HINTS+=("${_l#BACKED_UP: }")
    done <<< "$db_out"
  fi

  for t in "${TARGETS_TO_DO[@]}"; do
    tdir="$TARGETS_DIR/$t"

    pkgs=()
    for key in "${!PLAN[@]}"; do
      [[ "${key%%|*}" == "$t" ]] && pkgs+=("${key#*|}")
    done
    if [[ "${pkgs[@]+"${#pkgs[@]}"}" -eq 0 ]]; then
      info "目标 $t：source 里没有它要的包，整个目标跳过（也不执行 sh）"
      continue
    fi
    mapfile -t pkgs < <(printf '%s\n' "${pkgs[@]}" | sort)

    info "===== 目标 $t（${pkgs[*]}）====="
    t_failed=0

    # ---- 第 1 步：快照（当前线上包 -> 追加式还原点）----
    unset SNAP_OF; declare -A SNAP_OF=()
    for pkg in "${pkgs[@]}"; do
      if [[ $DRY_RUN -eq 1 ]]; then
        if [[ -f "$tdir/$pkg" ]]; then
          info "[演练] 快照 $pkg -> $pkg.$(date +%Y%m%d-%H%M%S)"
        else
          info "[演练] $pkg 首次发布（无旧包，无还原点）"
        fi
        continue
      fi
      snapshot_pkg "$tdir" "$pkg"; rrc=$?
      case "$rrc" in
        0) ok "快照 $pkg -> $SNAP_NAME"; SNAP_OF["$pkg"]="$SNAP_NAME" ;;
        1) ok "$pkg 首次发布（无旧包，无还原点）" ;;
        *) err "目标 $t：快照 $pkg 失败"; t_failed=1; break ;;
      esac
    done
    if [[ $t_failed -eq 1 ]]; then
      results+=("[FAIL] $t：快照失败（目标未动）")
      FAILED=1
      if [[ $KEEP_GOING -eq 1 ]]; then continue; else break; fi
    fi

    # ---- 第 2 步：替换包（临时文件 + sha256 + 原子改名）----
    deployed=()
    for pkg in "${pkgs[@]}"; do
      src="${PLAN[$t|$pkg]}"
      if [[ $DRY_RUN -eq 1 ]]; then
        info "[演练] 发布 $src -> $tdir/$pkg（sha256 $(sha256_of "$src" | cut -c1-12)...）"
        continue
      fi
      if copy_verified "$src" "$tdir" "$pkg"; then
        ok "发布 $pkg（sha256 校验一致）"
        deployed+=("$pkg")
      else
        err "目标 $t：发布 $pkg 失败"
        t_failed=1; break
      fi
    done
    if [[ $t_failed -eq 1 ]]; then
      for pkg in ${deployed[@]+"${deployed[@]}"}; do
        if [[ -n "${SNAP_OF[$pkg]:-}" ]] && copy_verified "$tdir/${SNAP_OF[$pkg]}" "$tdir" "$pkg"; then
          warn "已用还原点 ${SNAP_OF[$pkg]} 恢复 $pkg"
        elif [[ -n "${SNAP_OF[$pkg]:-}" ]]; then
          err "$pkg 恢复失败！请手动检查 $tdir/$pkg"
        else
          warn "$pkg 没有还原点（首次发布？），保留新包"
        fi
      done
      results+=("[FAIL] $t：发布失败（已尝试恢复到发布前状态）")
      FAILED=1
      if [[ $KEEP_GOING -eq 1 ]]; then continue; else break; fi
    fi
    if [[ $DRY_RUN -eq 1 ]]; then
      if resolve_sh "$tdir"; then
        if [[ $SH_NOARGS -eq 1 ]]; then
          info "[演练] 将执行 $t/$SH_NAME（不传包名参数）"
        else
          info "[演练] 将执行 $t/$SH_NAME ${pkgs[*]}"
        fi
      else
        err "[演练] 目标 $t：无法确定发布脚本"
      fi
      results+=("[DRY ] $t"); continue
    fi

    # ---- 第 3 步：执行发布 sh ----
    if resolve_sh "$tdir"; then sh_rel="$SH_NAME"; else sh_rel=""; fi
    if [[ -z "$sh_rel" ]]; then
      local shlist="" sf
      for sf in "$tdir"/*.sh; do shlist+="$(basename "$sf") "; done
      err "目标 $t：无法确定发布脚本（目录里的 .sh 不唯一：${shlist:-（无）}）。在 targets/$t/.deploy-script 第 1 行写脚本名（可选第 2 行 no-args），或运行时用 DEPLOY_SH=脚本名 指定"
      results+=("[FAIL] $t：找不到发布 sh（包已发布，请补脚本后单独执行）")
      FAILED=1
      if [[ $KEEP_GOING -eq 1 ]]; then continue; else break; fi
    fi
    if [[ ! -s "$tdir/$sh_rel" ]]; then
      err "目标 $t：发布脚本 $sh_rel 是空文件"
      results+=("[FAIL] $t：发布脚本为空（包已发布）")
      FAILED=1
      if [[ $KEEP_GOING -eq 1 ]]; then continue; else break; fi
    fi

    if [[ $SH_NOARGS -eq 1 ]]; then
      info "执行 $t/$sh_rel（不传包名参数，超时 ${SH_TIMEOUT}s）"
    else
      info "执行 $t/$sh_rel ${pkgs[*]}（超时 ${SH_TIMEOUT}s，包名作为参数传入）"
    fi
    if run_deploy_sh "$tdir" "$sh_rel" "$SH_NOARGS" ${pkgs[@]+"${pkgs[@]}"}; then
      ok "目标 $t：$sh_rel 执行成功"
      results+=("[OK  ] $t")
      for pkg in "${pkgs[@]}"; do prune_snapshots "$tdir" "$pkg"; done
    else
      rc=$?
      err "目标 $t：$sh_rel 失败（exit $rc），开始回滚"
      for pkg in "${pkgs[@]}"; do
        if [[ -n "${SNAP_OF[$pkg]:-}" ]] && copy_verified "$tdir/${SNAP_OF[$pkg]}" "$tdir" "$pkg"; then
          warn "已用还原点 ${SNAP_OF[$pkg]} 恢复 $pkg"
        elif [[ -n "${SNAP_OF[$pkg]:-}" ]]; then
          err "$pkg 恢复失败！请立即人工检查 $tdir/$pkg"
        else
          warn "$pkg 没有还原点（首次发布），保留新包"
        fi
      done
      if run_deploy_sh "$tdir" "$sh_rel" "$SH_NOARGS" ${pkgs[@]+"${pkgs[@]}"}; then
        warn "目标 $t：已回滚到旧版本，$sh_rel 重跑成功"
        results+=("[WARN] $t：新版本启动失败，已回滚旧版本")
      else
        err "目标 $t：回滚后 $sh_rel 重跑仍失败！请立即人工介入：$tdir"
        results+=("[FAIL] $t：发布失败且回滚后仍失败")
      fi
      FAILED=1
      if [[ $KEEP_GOING -eq 1 ]]; then continue; else break; fi
    fi
  done

  print_summary ${results[@]+"${results[@]}"}
  if [[ $DRY_RUN -eq 1 ]]; then
    info "（演练结束，未改动任何文件）"
  elif [[ $FAILED -eq 0 ]]; then
    ok "全部目标发布成功。sources/ 里的包没有删除，确认稳定后可自行清理；旧还原点已按 KEEP_BACKUPS=$KEEP_BACKUPS 自动清理。"
    if [[ "${DB_SNAP_HINTS[@]+"${#DB_SNAP_HINTS[@]}"}" -gt 0 ]]; then
      info "测试通过后建议 sh backup.sh 存档新版本；不通过时回滚数据："
      local _h
      for _h in "${DB_SNAP_HINTS[@]}"; do
        info "  sh $DB_HOOK import $_h    （发布前原始快照）"
      done
      info "代码回滚：bash restore.sh（选发布前的包还原点）"
    fi
  else
    err "有目标失败或被回滚，详见上方日志。"
  fi
  [[ $FAILED -eq 0 ]] && exit 0 || exit 1
}

# ---------------- 手动备份模式 ----------------
# 从 manifest 读取「路径 目标名...」，对每个目标里真实存在的包做快照。
# 注意：不要求 sources/ 里有包——清单只是包的台账，备份的是 targets/ 里的线上包。
backup_plan() {
  local line path rest t pkg
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    [[ "$line" =~ ^[[:space:]]*$ ]] && continue
    [[ "$line" == \#* ]] && continue
    read -r path rest <<< "$line"
    [[ -n "$path" && -n "$rest" ]] || die "$MANIFEST 格式错误，每行应为：文件路径 目标名 [目标名2 ...] —— 问题行：$line"
    pkg="$(basename "${path#./}")"
    for t in $rest; do
      [[ " ${TARGETS_ALL[*]} " == *" $t "* ]] || die "$MANIFEST 里的目标不存在：'$t'（targets/ 下没有这个目录，是不是拼错了？）"
      echo "$t $pkg"
    done
  done < "$MANIFEST"
}

do_backup_cmd() {
  if [[ $DRY_RUN -eq 1 ]]; then
    info "== 演练模式：手动备份（不改动任何文件）=="
  else
    acquire_lock
    setup_log
    info "== 手动备份开始 =="
  fi

  [[ -f "$MANIFEST" ]] || die "--backup 需要发布清单 $MANIFEST（它是包的台账）。目录约定模式下不用手动备份——每次发布都会自动快照"
  local -a pairs=()
  mapfile -t pairs < <(backup_plan)
  [[ "${pairs[@]+"${#pairs[@]}"}" -gt 0 ]] || die "$MANIFEST 里没有有效条目"

  local t pkg tdir rc made=0
  local -a results=()
  for pair in "${pairs[@]}"; do
    t="${pair%% *}"; pkg="${pair#* }"
    if [[ "${TARGET_FILTER[@]+"${#TARGET_FILTER[@]}"}" -gt 0 ]]; then
      [[ " ${TARGET_FILTER[*]} " == *" $t "* ]] || continue
    fi
    tdir="$TARGETS_DIR/$t"
    if [[ ! -f "$tdir/$pkg" ]]; then
      info "目标 $t：$pkg 线上不存在（还没发布过？），跳过"
      continue
    fi
    if [[ $DRY_RUN -eq 1 ]]; then
      info "[演练] 快照 $t/$pkg -> $pkg.$(date +%Y%m%d-%H%M%S)"
      continue
    fi
    snapshot_pkg "$tdir" "$pkg"; rc=$?
    if [[ $rc -eq 0 ]]; then
      ok "快照 $t/$pkg -> $SNAP_NAME"
      prune_snapshots "$tdir" "$pkg"
      made=$((made + 1))
    else
      err "快照 $t/$pkg 失败"
      results+=("[FAIL] $t/$pkg")
    fi
  done

  # 数据库备份钩子：deploy 根目录有 backup-db.sh 就带上（模板见 templates/backup-mysql.sh）
  # 数据库的导入永远手动：--restore 只回滚包，不碰数据库
  local db_ok=1
  if [[ -f "$DB_HOOK" ]]; then
    if [[ $DRY_RUN -eq 1 ]]; then
      info "[演练] 将执行数据库备份钩子：sh $DB_HOOK backup"
    else
      info "== 数据库备份钩子：sh $DB_HOOK backup =="
      if sh "$DB_HOOK" backup; then
        ok "数据库备份完成（$DB_HOOK）"
      else
        err "数据库备份失败（$DB_HOOK），详见钩子输出"
        results+=("[FAIL] 数据库备份")
        db_ok=0
      fi
    fi
  fi

  [[ $DRY_RUN -eq 1 ]] && { info "（演练结束，未改动任何文件）"; exit 0; }
  [[ $made -gt 0 || -f "$DB_HOOK" ]] || die "没有可备份的包（所有目标的包都不存在），也没有 $DB_HOOK 数据库钩子"
  ok "手动备份完成：包新增 $made 个还原点（每包保留最近 KEEP_BACKUPS=$KEEP_BACKUPS 个）$([[ -f "$DB_HOOK" ]] && echo "；数据库备份已执行")"
  [[ $db_ok -eq 1 && "${results[@]+"${#results[@]}"}" -eq 0 ]] && exit 0 || exit 1
}

# ---------------- 还原模式 ----------------
# 把选定的还原点（包名.时间戳）恢复为线上包，并重跑发布 sh。
# 交互模式列出所有还原点选择；--restore <时间戳> 直接恢复。
print_snap_menu() { # 全局：tss（时间戳降序）、snap_lines（「时间戳|目标|包名」行）
  info "可用还原点（新 → 旧）："
  local i=1 ts e ets et epkg t plist tlist
  for ts in "${tss[@]}"; do
    tlist=""
    for t in "${TARGETS_TO_DO[@]}"; do
      plist=""
      for e in "${snap_lines[@]}"; do
        IFS='|' read -r ets et epkg <<< "$e"
        [[ "$ets" == "$ts" && "$et" == "$t" ]] || continue
        plist+="$epkg "
      done
      [[ -n "$plist" ]] && tlist+="$t(${plist% }) "
    done
    printf '  [%d] %s   %s\n' "$i" "$ts" "${tlist% }"
    i=$((i + 1))
  done
}

do_restore() {
  local t pkg ts tdir e ets et epkg
  local -a snap_lines=() tss=()

  for t in "${TARGETS_TO_DO[@]}"; do
    tdir="$TARGETS_DIR/$t"
    while read -r ts pkg; do
      [[ -n "$ts" ]] || continue
      snap_lines+=("$ts|$t|$pkg")
    done < <(list_snapshots "$tdir" | sort)
  done
  [[ "${snap_lines[@]+"${#snap_lines[@]}"}" -gt 0 ]] || die "没有找到任何还原点（命名形如 包名.YYYYMMDD-HHMMSS）。发布一次或 --backup 都会产生"

  mapfile -t tss < <(printf '%s\n' "${snap_lines[@]}" | cut -d'|' -f1 | sort -ur)

  local choice=""
  if [[ -n "$RESTORE_TS" ]]; then
    choice="$RESTORE_TS"
  elif [[ $DRY_RUN -eq 1 ]]; then
    print_snap_menu
    info "（演练未指定还原点：加时间戳可演练具体还原，如 --restore ${tss[0]}）"
    exit 0
  else
    [[ -t 0 ]] || die "--restore 需要指定还原点时间戳（当前不是交互终端）：bash release.sh --restore ${tss[0]}"
    print_snap_menu
    printf '选择编号或时间戳（回车取消）：'
    read -r choice
    [[ -n "$choice" ]] || { info "已取消"; exit 0; }
    if [[ "$choice" =~ ^[0-9]+$ ]]; then
      (( choice >= 1 && choice <= ${#tss[@]} )) || die "编号超出范围：$choice"
      choice="${tss[choice - 1]}"
    fi
  fi
  [[ "$choice" =~ ^[0-9]{8}-[0-9]{6}$ ]] || die "还原点时间戳格式不对：$choice（应为 YYYYMMDD-HHMMSS）"
  [[ " ${tss[*]} " == *" $choice "* ]] || die "还原点 $choice 不存在。可用的有：${tss[*]}"

  if [[ $DRY_RUN -eq 1 ]]; then
    info "== 演练模式：还原 $choice 的计划（不改动任何文件）=="
  else
    acquire_lock
    setup_log
    info "== 还原开始：$choice =="
  fi

  local sh_rel rc bad any=0
  local -a results=()
  FAILED=0

  for t in "${TARGETS_TO_DO[@]}"; do
    tdir="$TARGETS_DIR/$t"
    local -a rlist=()
    for e in "${snap_lines[@]}"; do
      IFS='|' read -r ets et epkg <<< "$e"
      [[ "$ets" == "$choice" && "$et" == "$t" ]] || continue
      rlist+=("$epkg")
    done
    if [[ "${rlist[@]+"${#rlist[@]}"}" -eq 0 ]]; then
      info "目标 $t：没有 $choice 的还原点，跳过"
      continue
    fi
    any=1
    mapfile -t rlist < <(printf '%s\n' "${rlist[@]}" | sort)
    info "===== 目标 $t：还原 ${rlist[*]} ====="
    bad=0

    for pkg in "${rlist[@]}"; do
      if [[ $DRY_RUN -eq 1 ]]; then
        info "[演练] 恢复 $pkg.$choice -> $pkg"
        continue
      fi
      # 还原前快照当前线上包：还原错了还能再切回来（覆盖不可逆，先留底）
      snapshot_pkg "$tdir" "$pkg"
      rrc=$?
      [[ $rrc -eq 0 ]] && info "还原前快照：$SNAP_NAME"
      if copy_verified "$tdir/$pkg.$choice" "$tdir" "$pkg"; then
        ok "恢复 $pkg（sha256 校验一致）"
        prune_snapshots "$tdir" "$pkg"
      else
        err "恢复 $pkg 失败！请手动检查 $tdir"
        bad=1
      fi
    done
    [[ $DRY_RUN -eq 1 ]] && bad=0

    if resolve_sh "$tdir"; then sh_rel="$SH_NAME"; else sh_rel=""; fi
    if [[ -n "$sh_rel" ]]; then
      if [[ $DRY_RUN -eq 1 ]]; then
        if [[ $SH_NOARGS -eq 1 ]]; then
          info "[演练] 执行 $t/$sh_rel（不传包名参数）"
        else
          info "[演练] 执行 $t/$sh_rel ${rlist[*]}"
        fi
      elif run_deploy_sh "$tdir" "$sh_rel" "$SH_NOARGS" ${rlist[@]+"${rlist[@]}"}; then
        ok "目标 $t：还原后 $sh_rel 执行成功"
      else
        rc=$?
        err "目标 $t：还原后 $sh_rel 执行失败（exit $rc），请人工检查"
        bad=1
      fi
    else
      warn "目标 $t：找不到发布 sh，只恢复文件不重启"
    fi

    if [[ $bad -eq 0 ]]; then
      results+=("[OK  ] $t")
    else
      results+=("[FAIL] $t")
      FAILED=1
      if [[ $KEEP_GOING -eq 1 ]]; then continue; else break; fi
    fi
  done

  [[ $any -eq 1 ]] || die "没有目标拥有还原点 $choice（可用：${tss[*]}）"
  print_summary ${results[@]+"${results[@]}"}
  if [[ $DRY_RUN -eq 1 ]]; then
    info "（演练结束，未改动任何文件）"
  elif [[ $FAILED -eq 0 ]]; then
    ok "还原完成。被还原的版本还在还原点列表里，可随时再切。"
    if [[ -f "$DB_HOOK" ]]; then
      info "注意：还原只回滚了包。数据库如需一并回滚，用发布前快照手动导入：sh $DB_HOOK import <db-backups/里的对应文件>"
    fi
  fi
  [[ $FAILED -eq 0 ]] && exit 0 || exit 1
}

case "$MODE" in
  backup)  do_backup_cmd ;;
  restore) do_restore ;;
  *)       do_deploy ;;
esac
