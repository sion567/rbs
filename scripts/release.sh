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
#              ③ 【推荐】清单 sources/manifest.txt，明确写每个包发到哪，
#                 详见下方「发布清单」一节。
#              同一个包名同时出现在平铺和子目录、且两者会发到同一个
#              目标时，脚本报错拒绝（无法判断该用哪个文件）。
#   targets/   每个子目录是一个发布目标，放着当前版本的包 + 发布脚本 sh。
#              新目标的目录（含发布 sh）要先手工建好，脚本不会替你创建。
#              发布脚本按以下顺序确定：
#              ① targets/<目标名>/.deploy-script（一行写脚本名，目录里
#                 有多个 .sh 时靠它指定）
#              ② 环境变量 DEPLOY_SH=xxx.sh（所有目标统一改名）
#              ③ restart.sh（默认名，用 sh restart.sh 方式执行）
#              ④ 目录里唯一的一个 *.sh（自动识别，叫什么名字都行）
#
# 发布清单 sources/manifest.txt（推荐）：
#   格式：每行一条「文件路径 目标名 [目标名2 ...]」
#         - 路径相对 sources/（子目录里的包写 web/dist.tar.gz 这种相对路径）
#         - 目标名必须是 targets/ 下真实存在的目录，一行可写多个
#         - 空格 / Tab 分隔均可；# 开头为注释行；空行忽略；CRLF 也能读
#   示例（sources/manifest.txt 内容）：
#         # 后端 jar，首发也直接平铺放
#         xx-system-server.jar     xx-system
#         # 手机端、web 端同名包，各发各的
#         web/dist.tar.gz          web
#         mobile/dist.tar.gz       mobile
#         # 同一个包一次发多台
#         agent.jar                app1 app2
#   规则：
#   - 清单存在时它就是完整分发计划：指到哪发到哪，不再要求目标里有
#     同名旧包（首发平铺也能用）、同名包也不会串。
#   - sources/ 里的每个包（平铺 + 一级子目录）都必须列进清单，漏列
#     报错拒发（防漏发）。
#   - 这些情况直接报错：目标名拼错、路径写错、重复列同一个文件、
#     清单为空。删掉清单即回退到上面的目录约定规则。
#
# 每个目标的发布流程（每一步都有校验）：
#   1. 备份   旧包 xxx -> xxxYYYYMMDD（当天日期，直接接在文件名末尾）
#             - 旧包不存在（首次发布）-> 跳过备份
#             - 当天已成功发布过（目标目录里有 .release-ok-当天日期 标记）
#               或当天备份已存在 -> 跳过备份：
#               保留的始终是"上一个安全版本"，当天二次发布不会把
#               当天发出去的版本备份成安全版本
#             - 备份后做 sha256 校验，与原文件不一致则删除备份并报错
#   2. 替换   source 包先拷成目标目录里的隐藏临时文件，sha256 校验一致后
#             原子改名顶上；顶上后再次 sha256 校验
#   3. 执行   cd 到目标目录执行发布 sh（包名作为参数传给 sh）
#             - sh 失败 -> 自动用当天备份恢复旧包（逐包 sha256 校验），
#               重跑一次 sh；重跑仍失败则 loudly 报错终止
#
# 其他安全项：开始前校验 source 非空、每个包都能对上目标、磁盘剩余空间；
# flock 防并发；全程写日志到 logs/；--dry-run 演练不改任何文件。
#
# 用法：
#   bash release.sh                  正式发布（targets/ 下所有目标）
#   bash release.sh --targets a b    只发布指定目标
#   bash release.sh --dry-run        演练：只打印计划
#   bash release.sh --keep-going     某目标失败后继续其余目标（默认立即终止）
#   bash release.sh --rollback       回滚：用当天备份恢复并重跑 sh
#   bash release.sh --rollback --date 20261024   回滚到指定日期的备份
#
# 可用环境变量覆盖：SOURCE_DIR  TARGETS_DIR  DEPLOY_SH
#                   SH_TIMEOUT=600（发布 sh 超时秒数，0=不限）  MIN_FREE_MB=200
#
# 依赖：bash 4+、coreutils（sha256sum/cp/mv/df）；flock、timeout 可选。
# 约定：目录名 / 包名不要带空格。首次放到服务器后：chmod +x release.sh
# ============================================================================

set -uo pipefail

cd "$(dirname "$0")" || { echo "无法进入脚本所在目录" >&2; exit 1; }

SOURCE_DIR="${SOURCE_DIR:-sources}"
MANIFEST_NAME="${MANIFEST_NAME:-manifest.txt}"   # sources/ 下可选的发布清单
TARGETS_DIR="${TARGETS_DIR:-targets}"
DEPLOY_SH="${DEPLOY_SH:-restart.sh}"
SH_TIMEOUT="${SH_TIMEOUT:-600}"
MIN_FREE_MB="${MIN_FREE_MB:-200}"
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
  --rollback        回滚模式：用备份恢复旧包并重跑发布 sh
  --date YYYYMMDD   配合 --rollback 指定用哪天的备份（默认当天）
【推荐】发布清单 sources/manifest.txt：每行「文件路径 目标名...」（# 注释，路径相对 sources/）
  例：xx-system-server.jar  xx-system      首发（无同名旧包）也直接发
      web/dist.tar.gz       web            手机端/web 端同名包，各发各的
      agent.jar             app1 app2      一个包一次发多台
  清单存在时即完整分发计划：sources/ 里所有包必须都列出，漏列报错；删掉清单回退目录约定
无清单时的目录约定：平铺=发给有同名旧包的目标；子目录 sources/<目标名>/=只发该目标
首次发布：targets/<目标名>/（含发布 sh）要先手工建好
发布脚本：.deploy-script 文件指定 > DEPLOY_SH 环境变量 > restart.sh > 目录里唯一的 *.sh（执行方式：sh 脚本名 包名...）
环境变量：SOURCE_DIR / TARGETS_DIR / DEPLOY_SH / SH_TIMEOUT / MIN_FREE_MB / LOG_DIR
完整约定见脚本头部注释。
EOF
}

# ---------------- 参数解析 ----------------
MODE="deploy"; DRY_RUN=0; KEEP_GOING=0
DATE="$(date +%Y%m%d)"
TARGET_FILTER=()

while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run)    DRY_RUN=1 ;;
    --rollback)   MODE="rollback" ;;
    --keep-going) KEEP_GOING=1 ;;
    --date)
      [[ $# -ge 2 ]] || die "--date 需要一个 YYYYMMDD 参数"
      DATE="$2"; shift ;;
    --targets)
      shift
      while [[ $# -gt 0 && "$1" != -* ]]; do TARGET_FILTER+=("$1"); shift; done
      [[ ${#TARGET_FILTER[@]} -gt 0 ]] || die "--targets 后面要跟至少一个目标名"
      continue ;;
    -h|--help) usage; exit 0 ;;
    *) die "未知参数：$1（--help 看用法）" ;;
  esac
  shift
done
[[ "$DATE" =~ ^[0-9]{8}$ ]] || die "日期格式不对：$DATE（应为 YYYYMMDD）"

# ---------------- 全局预检 ----------------
[[ -d "$SOURCE_DIR"  ]] || die "找不到 ./$SOURCE_DIR/ 目录（应与本脚本同级）"
[[ -d "$TARGETS_DIR" ]] || die "找不到 ./$TARGETS_DIR/ 目录（应与本脚本同级）"
[[ $EUID -eq 0 ]] && warn "当前是 root 在跑，确认这是你的发布账号？"

shopt -s nullglob

TARGETS_ALL=()
for d in "$TARGETS_DIR"/*/; do
  TARGETS_ALL+=("$(basename "$d")")
done
[[ ${#TARGETS_ALL[@]} -gt 0 ]] || die "$TARGETS_DIR/ 下没有任何目标子目录"

if [[ ${#TARGET_FILTER[@]} -gt 0 ]]; then
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

# 备份旧包。返回 0=无需备份或已备份（原因写进全局 BACKUP_STATE），2=失败
do_backup() { # $1=tdir $2=pkg
  local tdir="$1" pkg="$2" cur="$1/$2" bak="$1/${2}${DATE}" mark="$1/.release-ok-$DATE" want got
  BACKUP_STATE=""
  if [[ ! -f "$cur" ]]; then
    BACKUP_STATE="旧包不存在（首次发布），无需备份"
    return 0
  fi
  if [[ -f "$mark" && "$(grep -Fx -- "$pkg" "$mark" 2>/dev/null)" == "$pkg" ]]; then
    BACKUP_STATE="今天已成功发布过 $pkg，当天再发不重复备份（不把今天的版本当成安全版本）"
    return 0
  fi
  if [[ -e "$bak" ]]; then
    BACKUP_STATE="今天的备份已存在（保留的是上一个安全版本），跳过"
    return 0
  fi
  want="$(sha256_of "$cur")" || { err "无法读取待备份文件：$cur"; return 2; }
  cp -p -- "$cur" "$bak" || { err "备份拷贝失败：$cur -> $bak"; return 2; }
  got="$(sha256_of "$bak")"
  [[ "$want" == "$got" ]] || { err "备份 sha256 不一致，已删除：$bak"; rm -f -- "$bak"; return 2; }
  BACKUP_STATE="已备份为 ${pkg}${DATE}（sha256 校验一致）"
  return 0
}

# 用当天备份恢复旧包。返回 0=已恢复 1=没有备份 2=恢复失败
restore_backup() { # $1=tdir $2=pkg
  local tdir="$1" pkg="$2" bak="$1/${2}${DATE}"
  [[ -f "$bak" ]] || return 1
  copy_verified "$bak" "$tdir" "$pkg" || return 2
  return 0
}

# 找发布脚本，优先级：
#   ① targets/<t>/.deploy-script（一行写脚本名；目录里有多个 .sh 时靠它指定）
#   ② 环境变量 DEPLOY_SH（全局统一改名）
#   ③ restart.sh（默认名，用 sh restart.sh 方式执行）
#   ④ 目录里唯一的 *.sh（自动识别，叫什么都行）
resolve_sh() { # $1=tdir
  local tdir="$1" cand=() name
  if [[ -f "$tdir/.deploy-script" ]]; then
    IFS= read -r name < "$tdir/.deploy-script" || true
    name="${name%$'\r'}"
    name="${name#"${name%%[![:space:]]*}"}"
    name="${name%"${name##*[![:space:]]}"}"
    [[ -n "$name" ]] || die "$tdir/.deploy-script 是空的，应写一行脚本名"
    [[ -f "$tdir/$name" ]] || die "$tdir/.deploy-script 指向的 '$name' 不存在（写相对该目标目录的文件名）"
    echo "$name"; return 0
  fi
  if [[ -f "$tdir/$DEPLOY_SH" ]]; then echo "$DEPLOY_SH"; return 0; fi
  cand=( "$tdir"/*.sh )
  if [[ ${#cand[@]} -eq 1 ]]; then basename "${cand[0]}"; return 0; fi
  return 1
}

run_deploy_sh() { # $1=tdir $2=sh_rel 其余=包名...
  local tdir="$1" sh_rel="$2"; shift 2
  (
    cd "$tdir" || exit 97
    if [[ "$SH_TIMEOUT" != "0" ]] && command -v timeout >/dev/null 2>&1; then
      exec timeout -k 10 "$SH_TIMEOUT" sh "./$sh_rel" "$@"
    else
      exec sh "./$sh_rel" "$@"
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
}

print_summary() { # $@=结果行
  echo
  info "================ 结果汇总 ================"
  if [[ $# -gt 0 ]]; then printf '%s\n' "$@"; else info "（无）"; fi
}

# ---------------- source -> targets 映射（仅发布模式） ----------------
declare -A PLAN   # "目标|包名" -> source 文件路径

MANIFEST="$SOURCE_DIR/$MANIFEST_NAME"

# 清单模式：sources/manifest.txt 存在时，它就是完整分发计划。
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
    [[ -f "$SOURCE_DIR/$path" ]] || die "$MANIFEST 里的文件不存在：sources/$path"
    [[ -z "${seen[$path]:-}" ]] || die "$MANIFEST 里重复列了同一个文件：sources/$path"
    seen["$path"]=1
    for t in $rest; do
      [[ " ${TARGETS_ALL[*]} " == *" $t "* ]] || die "$MANIFEST 里的目标不存在：'$t'（targets/ 下没有这个目录，是不是拼错了？）"
      PLAN["$t|$(basename "$path")"]="$SOURCE_DIR/$path"
    done
  done < "$MANIFEST"

  [[ ${#seen[@]} -gt 0 ]] || die "$MANIFEST 是空的。要么把清单写好，要么删掉它改用目录约定"

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
  for rel in "${!files[@]}"; do
    [[ -n "${seen[$rel]:-}" ]] || die "sources/$rel 没有出现在 $MANIFEST 里（清单模式下必须逐个列明，防止漏发）"
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

  [[ ${#PLAN[@]} -gt 0 ]] || die "$SOURCE_DIR/ 里没有任何可发布的文件"
}

build_plan() {
  if [[ -f "$MANIFEST" ]]; then
    build_plan_from_manifest
  else
    build_plan_by_name
  fi

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
  FAILED=0

  for t in "${TARGETS_TO_DO[@]}"; do
    tdir="$TARGETS_DIR/$t"

    pkgs=()
    for key in "${!PLAN[@]}"; do
      [[ "${key%%|*}" == "$t" ]] && pkgs+=("${key#*|}")
    done
    if [[ ${#pkgs[@]} -eq 0 ]]; then
      info "目标 $t：source 里没有它要的包，整个目标跳过（也不执行 sh）"
      continue
    fi
    mapfile -t pkgs < <(printf '%s\n' "${pkgs[@]}" | sort)

    info "===== 目标 $t（${pkgs[*]}）====="
    t_failed=0

    # ---- 第 1 步：备份（逐包校验）----
    for pkg in "${pkgs[@]}"; do
      if [[ $DRY_RUN -eq 1 ]]; then
        if [[ ! -f "$tdir/$pkg" ]]; then
          info "[演练] 备份 $pkg：旧包不存在，跳过"
        elif [[ -f "$tdir/.release-ok-$DATE" && -n "$(grep -Fx -- "$pkg" "$tdir/.release-ok-$DATE" 2>/dev/null)" ]]; then
          info "[演练] 备份 $pkg：今天已成功发布过，不重复备份"
        elif [[ -e "$tdir/${pkg}${DATE}" ]]; then
          info "[演练] 备份 $pkg：今日备份已存在，跳过（保留安全版本）"
        else
          info "[演练] 备份 $pkg -> ${pkg}${DATE}"
        fi
        continue
      fi
      if do_backup "$tdir" "$pkg"; then
        ok "备份 $pkg：$BACKUP_STATE"
      else
        err "目标 $t：备份 $pkg 失败"
        t_failed=1; break
      fi
    done
    if [[ $t_failed -eq 1 ]]; then
      results+=("[FAIL] $t：备份失败（目标未动）")
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
        restore_backup "$tdir" "$pkg"; rrc=$?
        case "$rrc" in
          0) warn "已用备份恢复 $pkg" ;;
          1) warn "$pkg 没有可用备份（首次发布？），保留新包" ;;
          *) err "$pkg 恢复失败！请手动检查 $tdir/$pkg" ;;
        esac
      done
      results+=("[FAIL] $t：发布失败（已尝试恢复到发布前状态）")
      FAILED=1
      if [[ $KEEP_GOING -eq 1 ]]; then continue; else break; fi
    fi
    if [[ $DRY_RUN -eq 1 ]]; then
      sh_rel="$(resolve_sh "$tdir" || true)"
      if [[ -n "$sh_rel" ]]; then
        info "[演练] 将执行 $t/$sh_rel ${pkgs[*]}"
      else
        err "[演练] 目标 $t：无法确定发布脚本"
      fi
      results+=("[DRY ] $t"); continue
    fi

    # ---- 第 3 步：执行发布 sh ----
    sh_rel="$(resolve_sh "$tdir" || true)"
    if [[ -z "$sh_rel" ]]; then
      local shlist="" sf
      for sf in "$tdir"/*.sh; do shlist+="$(basename "$sf") "; done
      err "目标 $t：无法确定发布脚本（目录里的 .sh 不唯一：${shlist:-（无）}）。在 targets/$t/.deploy-script 里写一行脚本名，或运行时用 DEPLOY_SH=脚本名 指定"
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

    info "执行 $t/$sh_rel ${pkgs[*]}（超时 ${SH_TIMEOUT}s，包名作为参数传入）"
    if run_deploy_sh "$tdir" "$sh_rel" ${pkgs[@]+"${pkgs[@]}"}; then
      ok "目标 $t：$sh_rel 执行成功"
      results+=("[OK  ] $t")
      # 写"今天已成功发布"标记，同一天再发不重复备份
      printf '%s\n' "${pkgs[@]}" >> "$tdir/.release-ok-$DATE"
    else
      rc=$?
      err "目标 $t：$sh_rel 失败（exit $rc），开始回滚"
      for pkg in "${pkgs[@]}"; do
        restore_backup "$tdir" "$pkg"; rrc=$?
        case "$rrc" in
          0) warn "已用备份恢复 $pkg" ;;
          1) warn "$pkg 没有备份（首次发布），保留新包" ;;
          *) err "$pkg 恢复失败！请立即人工检查 $tdir/$pkg" ;;
        esac
      done
      if run_deploy_sh "$tdir" "$sh_rel" ${pkgs[@]+"${pkgs[@]}"}; then
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
    ok "全部目标发布成功。sources/ 里的包没有删除，确认稳定后可自行清理；旧备份按需保留。"
  else
    err "有目标失败或被回滚，详见上方日志。"
  fi
  [[ $FAILED -eq 0 ]] && exit 0 || exit 1
}

# ---------------- 回滚模式 ----------------
do_rollback() {
  if [[ $DRY_RUN -eq 1 ]]; then
    info "== 演练模式：回滚到 $DATE 的备份（不改动任何文件）=="
  else
    acquire_lock
    setup_log
    info "== 回滚开始：使用 $DATE 的备份 =="
  fi

  local t tdir f name live bad sh_rel rc
  local -a restore_list results
  local found_any=0
  FAILED=0

  for t in "${TARGETS_TO_DO[@]}"; do
    tdir="$TARGETS_DIR/$t"
    restore_list=()
    for f in "$tdir"/*; do
      [[ -f "$f" ]] || continue
      name="$(basename "$f")"
      [[ "$name" == *"$DATE" ]] || continue
      live="${name%"$DATE"}"
      [[ -n "$live" ]] || continue
      restore_list+=("$live")
    done
    if [[ ${#restore_list[@]} -eq 0 ]]; then
      info "目标 $t：没有 $DATE 的备份，跳过"
      continue
    fi
    found_any=1
    mapfile -t restore_list < <(printf '%s\n' "${restore_list[@]}" | sort)
    info "===== 目标 $t：回滚 ${restore_list[*]} ====="
    bad=0

    for live in "${restore_list[@]}"; do
      if [[ $DRY_RUN -eq 1 ]]; then
        info "[演练] 恢复 ${live}${DATE} -> $live"
        continue
      fi
      restore_backup "$tdir" "$live"; rc=$?
      case "$rc" in
        0) ok "恢复 $live（sha256 校验一致）" ;;
        1) warn "$live 的备份不见了"; bad=1 ;;
        *) err "恢复 $live 失败！请手动检查 $tdir"; bad=1 ;;
      esac
    done
    [[ $DRY_RUN -eq 1 ]] && bad=0

    sh_rel="$(resolve_sh "$tdir" || true)"
    if [[ -n "$sh_rel" ]]; then
      if [[ $DRY_RUN -eq 1 ]]; then
        info "[演练] 执行 $t/$sh_rel ${restore_list[*]}"
      elif run_deploy_sh "$tdir" "$sh_rel" ${restore_list[@]+"${restore_list[@]}"}; then
        ok "目标 $t：回滚后 $sh_rel 执行成功"
      else
        rc=$?
        err "目标 $t：回滚后 $sh_rel 执行失败（exit $rc），请人工检查"
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

  [[ $found_any -eq 1 ]] || die "没有任何目标存在 $DATE 的备份，无事可做"
  print_summary ${results[@]+"${results[@]}"}
  if [[ $DRY_RUN -eq 0 && $FAILED -eq 0 ]]; then
    ok "回滚完成。注意：回滚不改写发布标记，当天再发新版也不会重复备份。"
  fi
  [[ $FAILED -eq 0 ]] && exit 0 || exit 1
}

if [[ "$MODE" == "rollback" ]]; then do_rollback; else do_deploy; fi
