#!/bin/sh
# restart.sh —— 非 docker 方式的 Java 服务重启（由 release.sh 在包发布完后自动调用）
# ---------------------------------------------------------------------------
# 调用方式：① release.sh 自动调用：cd 到目标目录后执行 sh restart.sh <包名>...
#           ② 手动重启（不发新包、不带参数）：sh targets/<目标名>/restart.sh
# 返回值：  新进程没起来 / 健康探测超时 -> 退出码非 0；
#           release.sh 据此自动把包回滚到上一个安全版本，并重新执行本脚本
# 语法：    纯 POSIX sh（release.sh 用 sh restart.sh 方式执行，
#           不要用 bash 专属语法：[[ ]]、数组、== 等）
# 使用：    复制到每个 targets/<目标名>/restart.sh，改下方【按项目改】几行。
#           约定一个目标目录 = 一个服务，进程记录在目录内 app.pid。
#
# 部署方式说明：裸进程 nohup 方式——release.sh 已先把新包原子替换到本目录，
# 这里停旧进程、起新进程即可，天然就是新包。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可被环境变量覆盖，如 HEALTH_URL=... sh restart.sh xxx.jar）=====
APP_PORT="${APP_PORT:-8080}"                        # 仅备注用；健康探测走 HEALTH_URL
JRE_BIN="${JRE_BIN:-java}"                          # java 可执行文件，如 /usr/local/jdk21/bin/java
HEALTH_URL="${HEALTH_URL:-}"                        # 启动探测地址，如 http://127.0.0.1:8080/actuator/health
                                                    # 留空 = 只检查进程是否存活；填了 = 等到返回 200 才算成功
WAIT_SECONDS="${WAIT_SECONDS:-60}"                  # HEALTH_URL 的最长等待秒数
JAVA_OPTS="${JAVA_OPTS:-}"                          # JVM 参数，如 "-Xms512m -Xmx1024m"
# ========================

APP_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_NAME="$(basename "$APP_DIR")"   # 服务名 = 目标目录名，如 xx-system
PID_FILE="$APP_DIR/app.pid"
LOG_FILE="$APP_DIR/app.log"

# 取包：优先用 release.sh 传入的第一个 .jar 参数（相对目标目录）；没传则用目录里的
JAR=""
for _a in "$@"; do
  case "$_a" in *.jar) JAR="$_a"; break ;; esac
done
if [ -n "$JAR" ]; then
  case "$JAR" in
    /*) JAR_PATH="$JAR" ;;
    *)  JAR_PATH="$APP_DIR/$JAR" ;;
  esac
else
  set -- "$APP_DIR"/*.jar
  if [ -f "$1" ]; then JAR_PATH="$1"; fi
fi
if [ -z "${JAR_PATH:-}" ] || [ ! -f "$JAR_PATH" ]; then
  echo "restart.sh：$APP_DIR 里找不到 jar 包（参数 $*）" >&2
  exit 1
fi

echo "== [1/3] 停止旧进程 =="
_stopped=0
if [ -f "$PID_FILE" ]; then
  _oldpid="$(cat "$PID_FILE" 2>/dev/null || true)"
  if [ -n "$_oldpid" ] && kill -0 "$_oldpid" 2>/dev/null; then
    echo "停止进程 $_oldpid（SIGTERM）..."
    kill "$_oldpid" 2>/dev/null || true
    _w=0
    while [ "$_w" -lt 10 ] && kill -0 "$_oldpid" 2>/dev/null; do
      sleep 1; _w=$((_w + 1))
    done
    if kill -0 "$_oldpid" 2>/dev/null; then
      echo "10 秒未退出，SIGKILL"
      kill -9 "$_oldpid" 2>/dev/null || true
    fi
    _stopped=1
  fi
  rm -f "$PID_FILE"
fi
# 兜底：清掉按 jar 路径匹配的残留进程（PID 文件丢失的场景）。
# 模式带 "-jar"：只杀 java 进程，避免误杀恰好打开了同路径文件的编辑器等
if command -v pgrep >/dev/null 2>&1; then
  for _pid in $(pgrep -f -- "-jar $JAR_PATH" 2>/dev/null); do
    [ "$_pid" != "$$" ] && kill "$_pid" 2>/dev/null || true
    _stopped=1
  done
fi
[ "$_stopped" -eq 1 ] && echo "旧进程已停止" || echo "（没有旧进程）"

echo "== [2/3] 启动新进程（$(basename "$JAR_PATH")）=="
cd "$APP_DIR"
# $JAVA_OPTS 不加引号是有意的（多个参数需要分词）
nohup "$JRE_BIN" $JAVA_OPTS -jar "$JAR_PATH" > "$LOG_FILE" 2>&1 &
echo $! > "$PID_FILE"
echo "已启动 PID $(cat "$PID_FILE")，日志：$LOG_FILE"

echo "== [3/3] 检查进程状态 =="
sleep 5
_newpid="$(cat "$PID_FILE" 2>/dev/null || true)"
if [ -z "$_newpid" ] || ! kill -0 "$_newpid" 2>/dev/null; then
  echo "restart.sh：进程启动后 5 秒内退出了！最近日志：" >&2
  tail -50 "$LOG_FILE" >&2 || true
  exit 1
fi
if [ -n "$HEALTH_URL" ] && command -v curl >/dev/null 2>&1; then
  _waited=0
  while [ "$_waited" -lt "$WAIT_SECONDS" ]; do
    if curl -sf -o /dev/null "$HEALTH_URL"; then break; fi
    sleep 5
    _waited=$((_waited + 5))
  done
  if [ "$_waited" -ge "$WAIT_SECONDS" ]; then
    echo "restart.sh：探测 $HEALTH_URL 超时（${WAIT_SECONDS}s），最近日志：" >&2
    tail -50 "$LOG_FILE" >&2 || true
    exit 1
  fi
  echo "健康检查通过：$HEALTH_URL"
fi
echo "$APP_NAME 部署完成，进程运行正常（PID $_newpid）"
