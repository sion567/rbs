#!/usr/bin/env bash
# selftest.sh —— release.sh 全场景回归测试（本地 Git Bash / 任意 Linux 都能跑）
# 用法：bash selftest.sh   （自动建临时沙箱、结束后清理；全部通过 exit 0）
# 改了 release.sh 之后跑一遍这个，比手工点验可靠。
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
REL="$HERE/release.sh"
PASS=0; FAIL=0; OUT=""; RC=0
SANDBOX="$(mktemp -d)"
S="$SANDBOX"   # 各场景沙箱路径
trap 'rm -rf "$SANDBOX"' EXIT

good() { PASS=$((PASS+1)); echo "PASS  $*"; }
bad()  { FAIL=$((FAIL+1)); echo "FAIL  $*"; }

# run <cwd> <cmd...>：执行并捕获输出到 OUT / RC
run() { local d="$1"; shift; OUT="$(cd "$d" && "$@" 2>&1)"; RC=$?; }

# fresh：重建一个干净场景目录（release.sh + sources/ + targets/）
fresh() {
  rm -rf "$S"
  mkdir -p "$S/sources" "$S/targets"
  cp "$REL" "$S/"
}

# mk_target <名> <restart退出码>：建目标目录，restart.sh 执行时往 calls.log 记一笔
mk_target() {
  mkdir -p "$S/targets/$1"
  printf '#!/bin/sh\necho "$1" >> calls.log\nexit %s\n' "${2:-0}" > "$S/targets/$1/restart.sh"
}

# 断言工具
chk_rc0()   { [[ $RC -eq 0 ]] && good "$1" || { bad "$1（RC=$RC）"; echo "$OUT" | tail -5; }; }
chk_rcne()  { [[ $RC -ne 0 ]] && good "$1" || bad "$1（应失败但 RC=0）"; }
chk_has()   { echo "$OUT" | grep -q "$1" && good "$2" || { bad "$2（输出里没有：$1）"; echo "$OUT" | tail -5; }; }
chk_file()  { [[ -e "$S/$1" ]] && good "$2" || bad "$2（$1 不存在）"; }
chk_nofile(){ [[ ! -e "$S/$1" ]] && good "$2" || bad "$2（$1 不应存在）"; }
chk_eq()    { [[ "$(cat "$S/$1" 2>/dev/null)" == "$2" ]] && good "$3" || bad "$3（$1 内容=$(cat "$S/$1" 2>/dev/null) 应为 $2）"; }
chk_glob()  { compgen -G "$S/$1" >/dev/null && good "$2" || bad "$2（无匹配：$1）"; }
chk_noglob(){ compgen -G "$S/$1" >/dev/null && bad "$2（不应有匹配：$1）" || good "$2"; }
chk_all_eq(){ local f allok=1; for f in "$S"/$1; do [[ -f "$f" && "$(cat "$f")" == "$2" ]] || allok=0; done; [[ $allok -eq 1 ]] && good "$3" || bad "$3"; }
# 还原点统计：目录里形如 包名.YYYYMMDD-HHMMSS 的文件
snap_count() { ls "$S/$1" 2>/dev/null | grep -cE '\.[0-9]{8}-[0-9]{6}$' | tr -d ' '; }
snap_oldest(){ local f; f="$(ls "$S/$1" 2>/dev/null | grep -E '\.[0-9]{8}-[0-9]{6}$' | sort | head -1)"; [[ -n "$f" ]] && echo "${f##*.}" || true; }

[[ -f "$REL" ]] || { echo "找不到 $REL"; exit 1; }
echo "== selftest：$(date '+%F %T') =="

# ---------- 1. dry-run 不改动任何文件 ----------
fresh
mk_target t1 0; echo v1 > "$S/targets/t1/app.jar"; echo v2 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh --dry-run
chk_rc0 "1. dry-run 退出码 0"
chk_eq "targets/t1/app.jar" "v1" "1. dry-run 未替换包"
chk_noglob "targets/t1/app.jar.*" "1. dry-run 未产生还原点"

# ---------- 2. 清单首发（目标无旧包） ----------
fresh
mk_target t1 0; echo v1 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh
chk_rc0 "2. 首发退出码 0"
chk_eq "targets/t1/app.jar" "v1" "2. 首发包已发布"
chk_noglob "targets/t1/app.jar.*" "2. 首发无还原点（符合预期）"

# ---------- 3. 连发两版：还原点追加不覆盖（熬夜跨天场景的关键） ----------
fresh
mk_target t1 0; echo v1 > "$S/targets/t1/app.jar"; echo v1 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh
echo v2 > "$S/sources/app.jar"
run "$S" bash release.sh
chk_rc0 "3. 二次发布退出码 0"
chk_eq "targets/t1/app.jar" "v2" "3. 线上是 v2"
[[ "$(snap_count "targets/t1")" == "2" ]] && good "3. 两次发布产生 2 个还原点" || bad "3. 还原点数量=$(snap_count "targets/t1")"
chk_all_eq "targets/t1/app.jar.*" "v1" "3. 还原点都是发布前的 v1（调试版不占安全位）"

# ---------- 4. sh 失败自动回滚 ----------
fresh
mk_target t1 3; echo v1 > "$S/targets/t1/app.jar"; echo v2 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh
chk_rcne "4. 发布失败退出码非 0"
chk_eq "targets/t1/app.jar" "v1" "4. 已回滚到 v1"
chk_all_eq "targets/t1/app.jar.*" "v1" "4. 还原点内容完整"
[[ "$(wc -l < "$S/targets/t1/calls.log" 2>/dev/null)" == "2" ]] && good "4. restart.sh 执行了 2 次（首发+回滚重跑）" || bad "4. restart.sh 调用次数不对：$(cat "$S/targets/t1/calls.log" 2>/dev/null | wc -l)"

# ---------- 5. --restore 还原到指定还原点 ----------
fresh
mk_target t1 0; echo v0 > "$S/targets/t1/app.jar"; echo v1 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh            # 还原点1 = v0
echo v2 > "$S/sources/app.jar"
run "$S" bash release.sh            # 还原点2 = v1，线上 v2
TS="$(snap_oldest "targets/t1")"
run "$S" bash release.sh --restore "$TS"
chk_rc0 "5. --restore 退出码 0"
chk_eq "targets/t1/app.jar" "v0" "5. 还原到最初的 v0"
[[ "$(wc -l < "$S/targets/t1/calls.log" 2>/dev/null)" == "3" ]] && good "5. restart.sh 共执行 3 次（2 次发布 + 1 次还原）" || bad "5. restart.sh 调用次数不对"

# ---------- 6. sources 出现清单外的包 → 拒发 ----------
fresh
mk_target t1 0; echo a1 > "$S/sources/a.jar"; echo b1 > "$S/sources/b.jar"
printf 'a.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh --dry-run
chk_rcne "6. 清单外包被拒"
chk_has "不在 manifest.txt" "6. 报错提示补清单行"

# ---------- 7. 清单目标名拼错 → 拒发 ----------
fresh
mk_target t1 0; echo a1 > "$S/sources/a.jar"
printf 'a.jar  t2\n' > "$S/manifest.txt"
run "$S" bash release.sh --dry-run
chk_rcne "7. 拼错目标被拒"
chk_has "目标不存在" "7. 报错提示目标不存在"

# ---------- 8. 清单里未上传的包自动跳过 ----------
fresh
mk_target t1 0; echo a1 > "$S/sources/a.jar"
printf 'a.jar  t1\nb.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh
chk_rc0 "8. 未上传包只跳过不报错"
chk_has "没上传，跳过" "8. 有跳过提示"
chk_nofile "targets/t1/b.jar" "8. 未上传的包没被发布"

# ---------- 9. 平铺与子目录同包名冲突 → 拒发 ----------
fresh
mk_target t1 0; mk_target t2 0
echo o1 > "$S/targets/t1/app.jar"; echo o2 > "$S/targets/t2/app.jar"
mkdir -p "$S/sources/t1"; echo sub > "$S/sources/t1/app.jar"; echo flat > "$S/sources/app.jar"
run "$S" bash release.sh --dry-run
chk_rcne "9. 冲突被拒"
chk_has "冲突" "9. 报错提示冲突"

# ---------- 10. --targets 只发指定目标 ----------
fresh
mk_target t1 0; mk_target t2 0
echo v1 > "$S/targets/t1/app.jar"; echo v1 > "$S/targets/t2/app.jar"; echo v2 > "$S/sources/app.jar"
printf 'app.jar  t1 t2\n' > "$S/manifest.txt"
run "$S" bash release.sh --targets t1
chk_rc0 "10. 过滤发布退出码 0"
chk_eq "targets/t1/app.jar" "v2" "10. t1 已更新"
chk_eq "targets/t2/app.jar" "v1" "10. t2 未被动过"

# ---------- 11. .deploy-script 指定脚本 ----------
fresh
mk_target t1 0
printf '#!/bin/sh\necho a > ran-a.txt\nexit 0\n' > "$S/targets/t1/a.sh"
printf '#!/bin/sh\necho b > ran-b.txt\nexit 0\n' > "$S/targets/t1/b.sh"
echo "b.sh" > "$S/targets/t1/.deploy-script"
echo v1 > "$S/targets/t1/app.jar"; echo v2 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh
chk_file "targets/t1/ran-b.txt" "11. 执行了 .deploy-script 指定的 b.sh"
chk_nofile "targets/t1/ran-a.txt" "11. 没有误跑 a.sh"

# ---------- 12. 目标缺发布 sh → FAIL ----------
fresh
mk_target t1 0; rm "$S/targets/t1/restart.sh"
echo v1 > "$S/targets/t1/app.jar"; echo v2 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh
chk_rcne "12. 缺 sh 时失败"
chk_has "无法确定发布脚本" "12. 报错说明怎么配"

# ---------- 13. sources 为空 → 人话报错 ----------
fresh
mk_target t1 0
printf 'app.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh --dry-run
chk_rcne "13. sources 为空失败"
chk_has "没有任何可发布的文件" "13. 人话报错"

# ---------- 14. 发布后内容逐字节一致 ----------
fresh
mk_target t1 0
head -c 4096 /dev/urandom > "$S/sources/blob.jar" 2>/dev/null || echo fake > "$S/sources/blob.jar"
printf 'blob.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh
if cmp -s "$S/sources/blob.jar" "$S/targets/t1/blob.jar"; then good "14. 内容一致"; else bad "14. 发布后内容不一致"; fi

# ---------- 15. docker 模板冒烟（假 docker） ----------
fresh
mkdir -p "$S/bin" "$S/targets/web"
cat > "$S/bin/docker" <<'EOF'
#!/bin/sh
case "$1" in
  rm) echo rm ;; run) echo run >> "$FAKE_LOG" ;; ps) echo cid ;; logs) echo log ;;
esac
EOF
chmod +x "$S/bin/docker"
cp "$HERE/templates/restart-docker.sh" "$S/targets/web/restart.sh"
echo jar-body > "$S/targets/web/app.jar"
FAKE_LOG="$S/fake.log" run "$S" env FAKE_LOG="$S/fake.log" PATH="$S/bin:$PATH" sh targets/web/restart.sh app.jar
chk_rc0 "15. docker 模板部署成功（假 docker）"
chk_file "fake.log" "15. docker run 被调用"

# ---------- 16. java 模板冒烟（假 java：前台运行 60s） ----------
fresh
mkdir -p "$S/bin" "$S/targets/svc"
cat > "$S/bin/java" <<'EOF'
#!/bin/sh
echo "started $(date +%s)" >> "$FAKE_LOG"
sleep 7
EOF
chmod +x "$S/bin/java"
cp "$HERE/templates/restart-java.sh" "$S/targets/svc/restart.sh"
echo jar-body > "$S/targets/svc/app.jar"
run "$S" env FAKE_LOG="$S/fake.log" PATH="$S/bin:$PATH" sh targets/svc/restart.sh app.jar
chk_rc0 "16. java 模板启动成功（假 java）"
chk_file "targets/svc/app.pid" "16. 写了 PID 文件"

# ---------- 17. java 模板：进程秒退 → 报失败 ----------
fresh
mkdir -p "$S/bin" "$S/targets/svc"
printf '#!/bin/sh\nexit 9\n' > "$S/bin/java"; chmod +x "$S/bin/java"
cp "$HERE/templates/restart-java.sh" "$S/targets/svc/restart.sh"
echo jar-body > "$S/targets/svc/app.jar"
run "$S" env PATH="$S/bin:$PATH" sh targets/svc/restart.sh app.jar
chk_rcne "17. 进程启动即死被检出"

# ---------- 18. 静态模板：解压到 WEB_ROOT + 检查 index.html ----------
fresh
mkdir -p "$S/targets/web" "$S/websrv/html" "$S/pkgroot"
echo '<html>v1</html>' > "$S/pkgroot/index.html"
tar -czf "$S/targets/web/dist.tar.gz" -C "$S/pkgroot" index.html
cp "$HERE/templates/restart-static.sh" "$S/targets/web/restart.sh"
run "$S" env WEB_ROOT="$S/websrv/html" sh targets/web/restart.sh dist.tar.gz
chk_rc0 "18. 静态包部署成功"
chk_eq "websrv/html/index.html" "<html>v1</html>" "18. index.html 已就位"

# ---------- 19. 参数模式：默认传包名；.deploy-script 第 2 行 no-args 不传 ----------
fresh
mkdir -p "$S/targets/t_def" "$S/targets/t_noargs"
printf '#!/bin/sh\necho $# > nargs.txt\nexit 0\n' > "$S/targets/t_def/restart.sh"
printf '#!/bin/sh\necho $# > nargs.txt\nexit 0\n' > "$S/targets/t_noargs/restart.sh"
printf 'restart.sh\r\nno-args\r\n' > "$S/targets/t_noargs/.deploy-script"   # 顺带测 CRLF
echo v1 > "$S/targets/t_def/app.jar"; echo v1 > "$S/targets/t_noargs/app.jar"
echo v2 > "$S/sources/app.jar"
printf 'app.jar  t_def t_noargs\n' > "$S/manifest.txt"
run "$S" bash release.sh
chk_rc0 "19. 混合参数模式发布成功"
chk_eq "targets/t_def/nargs.txt" "1" "19. 默认目标收到 1 个包名参数"
chk_eq "targets/t_noargs/nargs.txt" "0" "19. no-args 目标没收到参数（CRLF 兼容）"

# ---------- 20. .deploy-script 第 2 行写错 → 拒绝 ----------
fresh
mk_target t1 0
printf 'restart.sh\nbadflag\n' > "$S/targets/t1/.deploy-script"
echo v1 > "$S/targets/t1/app.jar"; echo v2 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh
chk_rcne "20. 非法第 2 行被拒"
chk_has "no-args" "20. 报错提示合法取值"

# ---------- 21. --backup 手动备份（sources 可以为空，只看 targets 线上包） ----------
fresh
mk_target t1 0
echo v1 > "$S/targets/t1/app.jar"; echo v1 > "$S/targets/t1/b.jar"
printf 'app.jar  t1\nb.jar  t1\n' > "$S/manifest.txt"
run "$S" bash release.sh --backup
chk_rc0 "21. --backup 成功（sources 为空也能备）"
chk_glob "targets/t1/app.jar.*" "21. app.jar 有了还原点"
chk_glob "targets/t1/b.jar.*" "21. b.jar 有了还原点"
chk_all_eq "targets/t1/app.jar.*" "v1" "21. 快照内容正确"

# ---------- 22. --restore 校验与演练 ----------
run "$S" bash release.sh --restore 20990101-000000
chk_rcne "22. 不存在的还原点被拒"
run "$S" bash release.sh --restore --dry-run
chk_rc0 "22. --restore --dry-run 列出还原点"

# ---------- 23. KEEP_BACKUPS 清理：超出保留数的最老还原点被删 ----------
fresh
mk_target t1 0
echo live > "$S/targets/t1/app.jar"
for ts in 20260101-000001 20260101-000002 20260101-000003; do
  echo "snap-$ts" > "$S/targets/t1/app.jar.$ts"
done
echo v2 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
run "$S" env KEEP_BACKUPS=2 bash release.sh
chk_rc0 "23. KEEP_BACKUPS=2 发布成功"
[[ "$(snap_count "targets/t1")" == "2" ]] && good "23. 还原点清理到 2 个" || bad "23. 还原点数量=$(snap_count "targets/t1")"
chk_noglob "targets/t1/app.jar.20260101-000001" "23. 最老的被清理"
chk_glob "targets/t1/app.jar.20260101-000003" "23. 较新的保留"
chk_eq "targets/t1/app.jar" "v2" "23. 线上正常"

# ---------- 24. --backup 的数据库钩子 ----------
fresh
mk_target t1 0; echo v1 > "$S/targets/t1/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
printf '#!/bin/sh\necho dbhook > dbhook.log\nexit 0\n' > "$S/backup-db.sh"
run "$S" bash release.sh --backup --dry-run
chk_rc0 "24. 钩子存在的 dry-run 正常"
chk_nofile "dbhook.log" "24. dry-run 不执行钩子"
run "$S" bash release.sh --backup
chk_rc0 "24. --backup 成功（包+钩子）"
chk_file "dbhook.log" "24. 数据库钩子被执行"
printf '#!/bin/sh\nexit 7\n' > "$S/backup-db.sh"
run "$S" bash release.sh --backup
chk_rcne "24. 钩子失败时 --backup 报失败"

# ---------- 25. 三个入口脚本（deploy/backup/restore）----------
fresh
mk_target t1 0; echo v1 > "$S/targets/t1/app.jar"; echo v2 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
cp "$HERE/deploy.sh" "$HERE/backup.sh" "$HERE/restore.sh" "$S/"
run "$S" sh deploy.sh --dry-run
chk_rc0 "25. deploy.sh 透传 --dry-run"
chk_eq "targets/t1/app.jar" "v1" "25. deploy.sh 演练未动文件"
run "$S" sh deploy.sh
chk_eq "targets/t1/app.jar" "v2" "25. deploy.sh 正式发布生效"
run "$S" sh restore.sh --dry-run
chk_rc0 "25. restore.sh 列出还原点"
run "$S" sh backup.sh
chk_rc0 "25. backup.sh 备份成功"

# ---------- 26. MySQL 模板全流程（假 docker：备份→校验→导入回环）----------
fresh
mkdir -p "$S/bin"
cat > "$S/bin/docker" <<'EOF'
#!/bin/sh
[ "$1" = "exec" ] || exit 0
shift
while [ $# -gt 0 ]; do
  case "$1" in
    -e) shift 2 ;;
    -i) shift ;;
    *) break ;;
  esac
done
case "$2" in
  mysqldump) printf -- '-- MySQL dump\n-- Dump completed\n'; exit 0 ;;
  mysql) cat > "$MYSQL_CAPTURE"; exit 0 ;;
esac
exit 0
EOF
chmod +x "$S/bin/docker"
cp "$HERE/templates/backup-mysql.sh" "$S/backup-db.sh"
run "$S" env PATH="$S/bin:$PATH" MYSQL_CONTAINER=c1 sh backup-db.sh backup
chk_rc0 "26. mysql 模板备份成功（假 docker）"
chk_glob "db-backups/appdb.*.sql.gz" "26. 备份文件已生成"
_gz="$(ls "$S"/db-backups/appdb.*.sql.gz 2>/dev/null | head -1)"
run "$S" env PATH="$S/bin:$PATH" MYSQL_CONTAINER=c1 MYSQL_CAPTURE="$S/imported.sql" FORCE=1 sh backup-db.sh import "$_gz"
chk_rc0 "26. mysql 模板导入成功（FORCE=1）"
grep -q -- "-- MySQL dump" "$S/imported.sql" 2>/dev/null && good "26. 导入内容与备份一致（回环校验）" || bad "26. 导入回环校验失败"

# ---------- 27. 发布前数据库原始快照（带 SQL 变更的发版流程） ----------
fresh
mk_target t1 0; echo v1 > "$S/targets/t1/app.jar"; echo v2 > "$S/sources/app.jar"
printf 'app.jar  t1\n' > "$S/manifest.txt"
printf '#!/bin/sh\necho "BACKED_UP: db-backups/appdb.20261028-100000.sql.gz"\necho dbhook > dbhook.log\nexit 0\n' > "$S/backup-db.sh"
run "$S" bash release.sh
chk_rc0 "27. 有钩子时发布成功"
chk_file "dbhook.log" "27. 发布前先备了库"
chk_has "import db-backups/appdb" "27. 结束时打印了数据回滚命令"
chk_has "bash restore.sh" "27. 结束时打印了代码回滚提示"
printf '#!/bin/sh\nexit 7\n' > "$S/backup-db.sh"
run "$S" bash release.sh
chk_rcne "27. 快照失败中止发布"
chk_eq "targets/t1/app.jar" "v2" "27. 中止时包未被动过（仍是上次发布的 v2）"
run "$S" env DB_HOOK_SOFT=1 bash release.sh
chk_rc0 "27. DB_HOOK_SOFT=1 硬发继续"

echo
echo "== 结果：PASS=$PASS FAIL=$FAIL =="
[[ $FAIL -eq 0 ]] || { echo "存在失败项"; exit 1; }
echo "全部通过"
