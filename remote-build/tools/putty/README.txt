内置 PuTTY 捆绑目录（设计文档 §5.4）
====================================

仓库不跟踪二进制文件。请从 https://www.putty.org 下载以下 5 个文件放入本目录：

  plink.exe       远端命令执行
  pscp.exe        文件上传/下载
  psftp.exe       SFTP 传输
  puttygen.exe    密钥转换（ppk <-> pem）
  pageant.exe     SSH agent（可选启动）
  VERSION         版本号文件（首行如 "0.79"）

缺失时 build.rs 会编译告警，运行期回退到系统 PATH 中的 PuTTY。
