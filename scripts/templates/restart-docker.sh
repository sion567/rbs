#!/bin/sh
# restart.sh —— docker 方式重启服务（由 release.sh 在包发布完后自动调用）
# ---------------------------------------------------------------------------
# 调用方式：① release.sh 自动调用：cd 到目标目录后执行 sh restart.sh <包名>...
#           ② 手动重启（不发新包、不带参数）：sh targets/<目标名>/restart.sh
#              —— 脚本按自己所在位置定位目标目录，在哪个目录执行都行
# 返回值：  容器没起来 / 健康探测超时 -> 退出码非 0；
#           release.sh 据此自动把 jar 回滚到上一个安全版本，并重新执行本脚本
# 语法：    纯 POSIX sh（release.sh 用 sh restart.sh 方式执行，
#           不要用 bash 专属语法：[[ ]]、数组、== 等）
# 使用：    复制到每个 targets/<目标名>/restart.sh，改下方【按项目改】几行。
#           约定一个目标目录 = 一个服务，容器名 = 目标目录名。
#
# 部署方式说明：挂载式——不构建镜像，宿主机上的 jar / 配置直接 -v 进 JRE
# 容器。release.sh 已先把新 jar 原子替换到本目录，所以这里只需删旧容器、
# 重新 run，天然就是新包。若想改成构建镜像方式，见文件末尾注释。
# ---------------------------------------------------------------------------

set -e

# ===== 【按项目改】（均可被环境变量覆盖，如 APP_PORT=9090 sh restart.sh xxx.jar）=====
APP_PORT="${APP_PORT:-8080}"                        # 对外端口（宿主机与容器同端口）
JRE_IMAGE="${JRE_IMAGE:-eclipse-temurin:8-jre}"     # JRE 镜像，按项目 JDK 版本改（17-jre / 21-jre ...）
HEALTH_URL="${HEALTH_URL:-}"                        # 启动探测地址，如 http://127.0.0.1:8080/actuator/health
                                                    # 留空 = 只检查容器是否运行；填了 = 等到返回 200 才算成功
WAIT_SECONDS="${WAIT_SECONDS:-60}"                  # HEALTH_URL 的最长等待秒数
JAVA_OPTS="${JAVA_OPTS:-}"                          # JVM 参数，如 "-Xms512m -Xmx1024m"
# ========================

# 目标目录 = 本脚本所在目录（release.sh 会 cd 进来执行；手动执行也能定位）
APP_DIR="$(cd "$(dirname "$0")" && pwd)"
APP_NAME="$(basename "$APP_DIR")"   # 容器名 = 目标目录名，如 xx-system

# 取 jar 名：优先用 release.sh 传入的第一个 .jar 参数（相对目标目录）；
# 没传则用目标目录里的 jar
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
  if [ -f "$1" ]; then
    JAR="$1"
    JAR_PATH="$1"
  fi
fi
if [ -z "${JAR_PATH:-}" ] || [ ! -f "$JAR_PATH" ]; then
  echo "restart.sh：$APP_DIR 里找不到 jar 包（参数 $*）" >&2
  exit 1
fi

echo "== [1/3] 删除旧容器 $APP_NAME =="
if docker rm -f "$APP_NAME" >/dev/null 2>&1; then
  echo "已删除旧容器"
else
  echo "（没有旧容器，跳过）"
fi

echo "== [2/3] 启动新容器（$JAR）=="
RUN_ARGS="-d --name $APP_NAME --restart=always -e TZ=Asia/Shanghai"
RUN_ARGS="$RUN_ARGS -p $APP_PORT:$APP_PORT"
RUN_ARGS="$RUN_ARGS -v $JAR_PATH:/app/app.jar:ro"
if [ -f "$APP_DIR/application.yml" ]; then
  # 挂到 /app/config/ 下，Spring Boot 会自动读取 ./config/application.yml
  RUN_ARGS="$RUN_ARGS -v $APP_DIR/application.yml:/app/config/application.yml:ro"
fi
# 下行 $RUN_ARGS / $JAVA_OPTS 不加引号是有意的（多个参数需要分词）
if ! docker run $RUN_ARGS --workdir /app "$JRE_IMAGE" java $JAVA_OPTS -jar /app/app.jar; then
  echo "restart.sh：docker run 失败！容器最近日志：" >&2
  docker logs --tail 50 "$APP_NAME" 2>&1 || true
  exit 1
fi

echo "== [3/3] 检查容器状态 =="
sleep 5
if [ -n "$HEALTH_URL" ] && command -v curl >/dev/null 2>&1; then
  _waited=0
  while [ "$_waited" -lt "$WAIT_SECONDS" ]; do
    if curl -sf -o /dev/null "$HEALTH_URL"; then break; fi
    sleep 5
    _waited=$((_waited + 5))
  done
  if [ "$_waited" -ge "$WAIT_SECONDS" ]; then
    echo "restart.sh：探测 $HEALTH_URL 超时（${WAIT_SECONDS}s），最近日志：" >&2
    docker logs --tail 50 "$APP_NAME" 2>&1 || true
    exit 1
  fi
  echo "健康检查通过：$HEALTH_URL"
fi
# 精确比对容器名：--filter name= 是子串匹配，xx-system 会匹配到 xx-system-v2
_running="$(docker ps --format '{{.Names}}' | grep -Fx "$APP_NAME")" || true
if [ -z "$_running" ]; then
  echo "restart.sh：容器没有处于运行状态！最近日志：" >&2
  docker logs --tail 50 "$APP_NAME" 2>&1 || true
  exit 1
fi
echo "$APP_NAME 部署完成，容器运行正常"

# 若想改成【构建镜像】方式：目标目录里放个 Dockerfile，把上面 [2/3] 换成：
#   docker build -t "$APP_NAME:latest" .
#   docker run -d --name "$APP_NAME" --restart=always -p $APP_PORT:$APP_PORT \
#     -e TZ=Asia/Shanghai "$APP_NAME:latest"
# 并在 run 后追加一行清理悬空镜像（可选）：
#   docker image prune -f
