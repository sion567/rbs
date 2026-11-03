//! 消息通知（Mattermost / 钉钉 / 邮箱）：作业结束时按渠道推送结果。
//!
//! 实现：零 Rust 依赖——起隐藏 PowerShell 发 HTTP / SMTP（`app.rs` 的 desktop_notify
//! 同款模式）。选 PowerShell 而非 HTTP 库的原因：
//! 1. 公司内网服务器常用自签证书（内部 CA），走 Windows 证书库天然可验，
//!    rustls 默认根证书不认，得额外配证书；
//! 2. 消息内容经**环境变量**传入脚本（不走命令行参数），彻底绕开转义与
//!    进程列表泄露凭据的问题；
//! 3. JSON payload 由 PS `ConvertTo-Json` 组装，无需手写 JSON 转义。
//!
//! 凭据（webhook URL / 钉钉 token+secret / SMTP 密码）一律存凭据管理器
//! （`secrets.rs`，键名见下方 KEY_* 常量），config.yaml 只留非敏感参数。

use crate::models::{DingTalkNotify, EmailNotify, GlobalConfig, MattermostNotify};
use crate::secrets;
use anyhow::{anyhow, Context, Result};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// 凭据管理器键名（settings UI 与发送共用）
// ---------------------------------------------------------------------------

pub const KEY_MATTERMOST: &str = "notify:mattermost";
pub const KEY_DINGTALK_TOKEN: &str = "notify:dingtalk-token";
pub const KEY_DINGTALK_SECRET: &str = "notify:dingtalk-secret";
pub const KEY_SMTP_PASSWORD: &str = "notify:smtp-password";

/// 发送超时（webhook / SMTP 通用）：测试按钮在 UI 线程同步等结果，不宜太长
const TIMEOUT_SECS: u64 = 15;

// ---------------------------------------------------------------------------
// 通知上下文与消息文案
// ---------------------------------------------------------------------------

/// 一次作业结束的通知上下文（GUI 事件消费点 / CLI 补发点构造）
#[derive(Debug, Clone)]
pub struct NotifyCtx {
    /// 「项目/流程」或组运行标签
    pub label: String,
    /// 状态全文（status.short()，含失败步骤与原因）
    pub status_text: String,
    pub ok: bool,
    pub duration_ms: u64,
    pub run_id: String,
}

impl NotifyCtx {
    /// 「[rbs] ❌ project/job 失败」——通知标题 / 邮件主题 / 钉钉 markdown title
    pub fn title(&self) -> String {
        format!("[rbs] {} {} {}", self.icon(), self.label, self.verdict())
    }

    fn icon(&self) -> &'static str {
        if self.ok {
            "✅"
        } else {
            "❌"
        }
    }

    fn verdict(&self) -> &'static str {
        if self.ok {
            "成功"
        } else {
            "失败"
        }
    }

    /// 正文（markdown 多行）：状态 / 耗时·主机 / 时间·run_id
    pub fn body(&self) -> String {
        format!(
            "状态: {}\n耗时: {} · 主机: {}\n时间: {} · run: {}",
            self.status_text,
            format_duration(self.duration_ms),
            host_name(),
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            self.run_id,
        )
    }
}

/// 耗时人性化：45秒 / 3分12秒 / 1时2分3秒
pub fn format_duration(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}秒")
    } else if s < 3600 {
        format!("{}分{}秒", s / 60, s % 60)
    } else {
        format!("{}时{}分{}秒", s / 3600, (s % 3600) / 60, s % 60)
    }
}

fn host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_else(|_| "-".into())
}

// ---------------------------------------------------------------------------
// 发送入口
// ---------------------------------------------------------------------------

/// 作业结束（GUI 侧）：按触发时机与各渠道开关推送。每渠道一个线程（互不阻塞，
/// 单渠道卡死不影响其余渠道与 UI）；发送是 spawn 即忘——失败打 stderr。
pub fn send_job_end(global: &GlobalConfig, ctx: &NotifyCtx) {
    dispatch(global, ctx, false);
}

/// 作业结束（CLI 侧）：同步逐渠道发送。CLI 发完立即退出进程，后台线程
/// 会被杀掉导致通知发不出去，必须阻塞等发送完成（每渠道自有 15s 超时兜底）。
pub fn send_job_end_blocking(global: &GlobalConfig, ctx: &NotifyCtx) {
    dispatch(global, ctx, true);
}

fn dispatch(global: &GlobalConfig, ctx: &NotifyCtx, inline: bool) {
    let notify = &global.notify;
    let should_send = if ctx.ok {
        notify.on_success
    } else {
        notify.on_failure
    };
    if !should_send {
        return;
    }
    if notify.mattermost.enabled {
        let cfg = notify.mattermost.clone();
        let ctx = ctx.clone();
        let run = move || {
            if let Err(e) = send_mattermost(&cfg, &ctx) {
                eprintln!("[rbs 通知] Mattermost 发送失败: {e:#}");
            }
        };
        if inline { run() } else { std::thread::spawn(run); }
    }
    if notify.dingtalk.enabled {
        let cfg = notify.dingtalk.clone();
        let ctx = ctx.clone();
        let run = move || {
            if let Err(e) = send_dingtalk(&cfg, &ctx) {
                eprintln!("[rbs 通知] 钉钉发送失败: {e:#}");
            }
        };
        if inline { run() } else { std::thread::spawn(run); }
    }
    if notify.email.enabled {
        let cfg = notify.email.clone();
        let ctx = ctx.clone();
        let run = move || {
            if let Err(e) = send_email(&cfg, &ctx) {
                eprintln!("[rbs 通知] 邮件发送失败: {e:#}");
            }
        };
        if inline { run() } else { std::thread::spawn(run); }
    }
}

/// Mattermost：POST {"text": ...} 到 incoming webhook（URL 存凭据管理器）。
/// 同步阻塞，供后台线程与设置窗口「发送测试」共用。
pub fn send_mattermost(_cfg: &MattermostNotify, ctx: &NotifyCtx) -> Result<String> {
    let url = secrets::get_password(KEY_MATTERMOST)?.filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("未配置 Webhook URL（设置窗口填入后自动存凭据管理器）"))?;
    let mut cmd = ps_command(PS_MATTERMOST);
    cmd.env("RBS_MM_URL", url.trim());
    cmd.env("RBS_TEXT", format!("{}\n\n{}", ctx.title(), ctx.body()));
    run_powershell(cmd)?;
    Ok("已发送".into())
}

/// 钉钉：POST markdown 消息到自定义机器人；配了加签 secret 则在脚本内
/// HMAC-SHA256 签名后拼 URL（时间戳由 PS 生成）。
pub fn send_dingtalk(_cfg: &DingTalkNotify, ctx: &NotifyCtx) -> Result<String> {
    let token = secrets::get_password(KEY_DINGTALK_TOKEN)?.filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("未配置 access_token（设置窗口填入后自动存凭据管理器）"))?;
    let mut cmd = ps_command(PS_DINGTALK);
    // access_token 可能带完整 URL 也可能只是裸 token：统一拼成带参 URL
    let token = token.trim();
    let url = if token.starts_with("http") {
        token.to_string()
    } else {
        format!("https://oapi.dingtalk.com/robot/send?access_token={token}")
    };
    cmd.env("RBS_DD_URL", url);
    if let Ok(Some(secret)) = secrets::get_password(KEY_DINGTALK_SECRET) {
        cmd.env("RBS_DD_SECRET", secret.trim());
    }
    cmd.env("RBS_TITLE", ctx.title());
    cmd.env("RBS_TEXT", format!("{}\n\n{}", ctx.title(), ctx.body()));
    run_powershell(cmd)?;
    Ok("已发送".into())
}

/// 邮箱：SMTP 直发。starttls（587）/ ssl（465 隐式 TLS，脚本内 SslStream
/// 手写 SMTP 对话）/ none（25 明文，内网中继）三种加密模式。
pub fn send_email(cfg: &EmailNotify, ctx: &NotifyCtx) -> Result<String> {
    if cfg.smtp_host.trim().is_empty() {
        return Err(anyhow!("未配置 SMTP 服务器地址"));
    }
    if cfg.to.is_empty() {
        return Err(anyhow!("未配置收件人"));
    }
    let password = secrets::get_password(KEY_SMTP_PASSWORD)?.unwrap_or_default();
    let from = if cfg.from.trim().is_empty() {
        cfg.username.trim().to_string()
    } else {
        cfg.from.trim().to_string()
    };
    if from.is_empty() {
        return Err(anyhow!("未配置发件人（from 或 username）"));
    }
    let script = mail_script(&cfg.ssl);
    let mut cmd = ps_command(&script);
    cmd.env("RBS_SMTP_HOST", cfg.smtp_host.trim());
    cmd.env("RBS_SMTP_PORT", cfg.smtp_port.to_string());
    cmd.env("RBS_SMTP_USERNAME", cfg.username.trim());
    cmd.env("RBS_SMTP_PASSWORD", password); // 环境变量传密码：不进命令行（进程列表不可见）
    cmd.env("RBS_SMTP_FROM", from);
    cmd.env("RBS_SMTP_TO", cfg.to.join(";"));
    cmd.env("RBS_TITLE", ctx.title());
    cmd.env("RBS_TEXT", format!("{}\n\n{}", ctx.title(), ctx.body()));
    run_powershell(cmd)?;
    Ok(format!("已发送至 {}", cfg.to.join(", ")))
}

// ---------------------------------------------------------------------------
// PowerShell 执行基建（desktop_notify 同款：隐藏窗口 + spawn 即忘 / 带超时等待）
// ---------------------------------------------------------------------------

fn ps_command(script: &str) -> Command {
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW：不闪黑框
    }
    cmd
}

/// 同步执行并等结果：超时杀进程（后台线程 / 测试按钮都走这里，
/// 不会把 UI 挂死在无响应的 webhook / SMTP 上）。
fn run_powershell(mut cmd: Command) -> Result<()> {
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().context("启动 PowerShell 失败")?;
    let start = Instant::now();
    loop {
        match child.try_wait()? {
            Some(status) => {
                // 进程已退出，管道不会再写入，此刻读干净即可
                let mut err = String::new();
                if let Some(mut se) = child.stderr.take() {
                    use std::io::Read as _;
                    let _ = se.read_to_string(&mut err);
                }
                if !status.success() {
                    // PS 异常的第一行是异常本体（如 Invoke-RestMethod : 404），
                    // 后续是 At line / CategoryInfo 噪音
                    let first = err.lines().map(str::trim).find(|l| !l.is_empty())
                        .unwrap_or("PowerShell 退出码非零");
                    return Err(anyhow!("{first}"));
                }
                return Ok(());
            }
            None if start.elapsed() >= Duration::from_secs(TIMEOUT_SECS) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow!("超时（{TIMEOUT_SECS}s 无响应）"));
            }
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

// ---------------------------------------------------------------------------
// PowerShell 脚本：消息内容全部经环境变量传入（RBS_*），脚本内零转义
// ---------------------------------------------------------------------------

/// Mattermost incoming webhook：ConvertTo-Json 组 payload，UTF-8 字节保中文
const PS_MATTERMOST: &str = r#"
$body = @{ text = $env:RBS_TEXT } | ConvertTo-Json
Invoke-RestMethod -Uri $env:RBS_MM_URL -Method Post -ContentType 'application/json; charset=utf-8' `
  -Body ([Text.Encoding]::UTF8.GetBytes($body)) -TimeoutSec 12 | Out-Null
"#;

/// 钉钉自定义机器人：markdown 消息；配了加签 secret 则 HMAC-SHA256 签名拼 URL。
/// errcode 非零时 HTTP 仍是 200，必须显式检查响应体
const PS_DINGTALK: &str = r#"
$url = $env:RBS_DD_URL
if ($env:RBS_DD_SECRET) {
  $ts = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
  $hmac = [System.Security.Cryptography.HMACSHA256]::new([Text.Encoding]::UTF8.GetBytes($env:RBS_DD_SECRET))
  $sign = [Convert]::ToBase64String($hmac.ComputeHash([Text.Encoding]::UTF8.GetBytes("$ts`n$env:RBS_DD_SECRET")))
  $url = "$url&timestamp=$ts&sign=$([Uri]::EscapeDataString($sign))"
}
$payload = @{ msgtype = 'markdown'; markdown = @{ title = $env:RBS_TITLE; text = $env:RBS_TEXT } } | ConvertTo-Json
$r = Invoke-RestMethod -Uri $url -Method Post -ContentType 'application/json; charset=utf-8' `
  -Body ([Text.Encoding]::UTF8.GetBytes($payload)) -TimeoutSec 12
if ($r.errcode -ne 0) { throw "钉钉接口报错: $($r.errmsg)" }
"#;

/// Send-MailMessage 公共参数（收件人拆分、密码转 SecureString、UTF-8 编码）；
/// 用户名+密码都非空才带 Credential（匿名中继不认证）
const PS_MAIL_PARAMS: &str = r#"
function MailParams {
  $p = @{
    SmtpServer = $env:RBS_SMTP_HOST; Port = [int]$env:RBS_SMTP_PORT
    From = $env:RBS_SMTP_FROM; To = ($env:RBS_SMTP_TO -split ';')
    Subject = $env:RBS_TITLE; Body = $env:RBS_TEXT
    Encoding = [Text.Encoding]::UTF8; ErrorAction = 'Stop'
  }
  if ($env:RBS_SMTP_USERNAME -and $env:RBS_SMTP_PASSWORD) {
    $sec = ConvertTo-SecureString $env:RBS_SMTP_PASSWORD -AsPlainText -Force
    $p.Credential = [pscredential]::new($env:RBS_SMTP_USERNAME, $sec)
  }
  $p
}
"#;

/// 587 STARTTLS：Send-MailMessage 开箱即用（PS 5.1 自带，内网 Exchange/Coremail 常规路径）
const PS_MAIL_STARTTLS: &str = r#"
$p = MailParams
$p.UseSsl = $true
Send-MailMessage @p
"#;

/// 25 明文（内网中继）：同上但不加密
const PS_MAIL_PLAIN: &str = r#"
$p = MailParams
Send-MailMessage @p
"#;

/// 按加密模式拼完整邮件脚本：公共参数函数 + 模式专属发送逻辑
fn mail_script(ssl_mode: &str) -> String {
    let mode = match ssl_mode {
        "ssl" => PS_MAIL_SSL,
        "none" => PS_MAIL_PLAIN,
        _ => PS_MAIL_STARTTLS,
    };
    format!("{PS_MAIL_PARAMS}\n{mode}")
}

/// 465 隐式 TLS：Send-MailMessage 不支持隐式 SSL，用 SslStream 手写最小 SMTP 对话
/// （EHLO → AUTH LOGIN → MAIL/RCPT/DATA → QUIT；正文 base64 编码免 dot-stuffing）
const PS_MAIL_SSL: &str = r#"
$tcp = [Net.Sockets.TcpClient]::new($env:RBS_SMTP_HOST, [int]$env:RBS_SMTP_PORT)
$ssl = [Net.Security.SslStream]::new($tcp.GetStream())
$ssl.AuthenticateAsClient($env:RBS_SMTP_HOST)
$rd = [IO.StreamReader]::new($ssl)
$wr = [IO.StreamWriter]::new($ssl)
$wr.AutoFlush = $true
$wr.NewLine = "`r`n"
function ReadReply {
  do { $r = $rd.ReadLine() } while ($r.Length -ge 4 -and $r[3] -eq '-')
  if ($r[0] -ne '2' -and $r[0] -ne '3') { throw "SMTP: $r" }
  $r
}
[void](ReadReply)                                    # 220 banner
$wr.WriteLine('EHLO remote-build'); [void](ReadReply)
$wr.WriteLine('AUTH LOGIN'); [void](ReadReply)       # 334
$wr.WriteLine([Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($env:RBS_SMTP_USERNAME))); [void](ReadReply)
$wr.WriteLine([Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($env:RBS_SMTP_PASSWORD))); [void](ReadReply)  # 235
$wr.WriteLine("MAIL FROM:<$($env:RBS_SMTP_FROM)>"); [void](ReadReply)
foreach ($t in ($env:RBS_SMTP_TO -split ';')) {
  $wr.WriteLine("RCPT TO:<$t>"); [void](ReadReply)
}
$wr.WriteLine('DATA'); [void](ReadReply)             # 354
$subj = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($env:RBS_TITLE))
$body64 = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($env:RBS_TEXT))
$wr.Write("From: $($env:RBS_SMTP_FROM)`r`nTo: $(($env:RBS_SMTP_TO -split ';') -join ', ')`r`nSubject: =?utf-8?B?$subj?=`r`nMIME-Version: 1.0`r`nContent-Type: text/plain; charset=utf-8`r`nContent-Transfer-Encoding: base64`r`n`r`n")
for ($i = 0; $i -lt $body64.Length; $i += 76) {
  $wr.WriteLine($body64.Substring($i, [Math]::Min(76, $body64.Length - $i)))
}
$wr.WriteLine('.'); [void](ReadReply)                # 250
$wr.WriteLine('QUIT')
$tcp.Close()
"#;

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(ok: bool) -> NotifyCtx {
        NotifyCtx {
            label: "demo/前端发布".into(),
            status_text: if ok { "success".into() } else { "failed (step 2: pnpm build 退出码 1)".into() },
            ok,
            duration_ms: 192_345,
            run_id: "demo-20261103-142305123".into(),
        }
    }

    #[test]
    fn title_and_body_shape() {
        let c = ctx(false);
        assert_eq!(c.title(), "[rbs] ❌ demo/前端发布 失败");
        let body = c.body();
        assert!(body.contains("状态: failed (step 2:"));
        assert!(body.contains("耗时: 3分12秒"));
        assert!(body.contains("run: demo-20261103-142305123"));
        let ok = ctx(true);
        assert_eq!(ok.title(), "[rbs] ✅ demo/前端发布 成功");
    }

    #[test]
    fn duration_formatting() {
        assert_eq!(format_duration(45_000), "45秒");
        assert_eq!(format_duration(192_345), "3分12秒");
        assert_eq!(format_duration(3_723_000), "1时2分3秒");
    }

    #[test]
    fn send_skips_when_trigger_off() {
        // 失败作业 + 仅 on_failure 开：send_job_end 不 panic、不启用渠道即静默返回
        let mut g = GlobalConfig::default();
        g.notify.on_failure = true;
        g.notify.on_success = false;
        send_job_end(&g, &ctx(false)); // 无启用渠道 → 什么都不发生
    }

    #[test]
    fn scripts_take_content_from_env_only() {
        // 脚本内容来自环境变量：模板里不含消息占位符，内容含引号/换行也无需转义
        let mail_modes = ["starttls", "ssl", "none", "未知模式兜底"];
        let scripts = [
            PS_MATTERMOST.to_string(),
            PS_DINGTALK.to_string(),
            PS_MAIL_PARAMS.to_string(),
            PS_MAIL_SSL.to_string(),
        ]
        .into_iter()
        .chain(mail_modes.iter().map(|m| mail_script(m)))
        .collect::<Vec<_>>();
        for s in scripts {
            assert!(!s.contains("{{"), "脚本不应内插消息内容: {s}");
            assert!(s.contains("RBS_"), "脚本应从环境变量取内容");
        }
    }
}
