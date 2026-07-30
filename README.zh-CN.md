# Herdr 远程文件下载

[English](README.md) | [日本語](README.ja.md) | 简体中文

这是一个 Herdr 插件，用于将 `herdr --remote` 远程主机上的文件下载到连接端
Mac 的 `~/Downloads`。

您可以通过提示字符选择当前屏幕中显示的文件路径，也可以传输 Codex、Claude
等工具显示的 `file://` 链接。它支持任何扩展名的常规文件，包括 PDF、PPTX、
图像和压缩包。

## 工作原理

Herdr 插件运行在远程主机上，因此需要在连接端 Mac 上启动一个小型接收服务。
传输仅使用 SSH `RemoteForward`，不会开放任何公网端口。文件选择器、发送端、
接收端和服务管理 CLI 均使用 Rust 实现。

```text
remote Herdr plugin
  -> /tmp/herdr-remote-download-<remote-user>.sock
  -> SSH RemoteForward
  -> 127.0.0.1:18340 on the connected Mac
  -> ~/Downloads
```

传输使用一个随机的 64 字符令牌进行认证，并在接收后通过 SHA-256 校验。
默认大小上限为 512 MiB。插件不会覆盖已有文件；同名文件会保存为
`name (1).ext` 等名称。

## 环境要求

- Herdr 0.7.0 或更高版本
- 连接端：macOS、Git、Rust/Cargo 1.88 或更高版本
- 远程主机：Linux 或 macOS、Git、Rust/Cargo 1.88 或更高版本
- 已在 SSH config 中定义的远程主机

## 安装设置

以下示例使用 `your-server` 作为现有的 SSH 主机名。

### 1. Mac 端接收服务

在 Mac 上克隆仓库并安装 launchd 服务。

```sh
git clone https://github.com/kosuketut/herdr-remotedownloder.git
cd herdr-remotedownloder
cargo build --release --locked
./target/release/herdr-remote-download install-service
./target/release/herdr-remote-download service-status
curl -fsS http://127.0.0.1:18340/health
```

接收服务仅绑定到 `127.0.0.1`，日志写入
`~/Library/Logs/herdr-remote-download.log`。认证令牌会创建在
`~/.config/herdr-remote-download/token`。

### 2. SSH 隧道

在 Mac 的 `~/.ssh/config` 中添加一个 Herdr 专用主机。请使用与普通 SSH
主机不同的名称，以隔离端口转发设置。请将此配置块放在 `Host *`之前。

```sshconfig
Host your-server-herdr
    HostName server.example.com
    User your-name
    IdentityFile ~/.ssh/id_ed25519
    IdentitiesOnly yes
    RemoteForward /tmp/herdr-remote-download-your-name.sock 127.0.0.1:18340
    ExitOnForwardFailure yes
    ServerAliveInterval 15
    ServerAliveCountMax 3
    ControlMaster no
    ControlPath none
```

请将套接字路径中的 `your-name`替换为远程主机上 `id -un`返回的用户名。
Unix 套接字可以防止新旧 SSH 会话共享同一个 TCP 转发端口。
如果无法创建套接字，`ExitOnForwardFailure yes`会在连接阶段停止 Herdr。

### 3. 在远程主机上安装插件

运行：

```sh
herdr plugin install kosuketut/herdr-remotedownloder
```

确认安装内容后，安装程序会根据插件清单构建两个 Rust 二进制文件。

### 4. 将认证令牌复制到远程主机

在 Mac 上运行：

```sh
ssh your-server \
  'config_dir=$(herdr plugin config-dir kosukeyano.remote-download) &&
   mkdir -p "$config_dir" &&
   umask 077 &&
   cat > "$config_dir/token"' \
  < ~/.config/herdr-remote-download/token
```

请勿将令牌添加到仓库，也不要与其他人共享。

### 5. 快捷键

将以下配置添加到远程主机的 `~/.config/herdr/config.toml`：

```toml
[[keys.command]]
key = "prefix+d"
type = "plugin_action"
command = "kosukeyano.remote-download.pick"
description = "pick a remote file to download"
```

重新加载配置：

```sh
herdr server reload-config
```

要在远程会话中使用服务端快捷键，请在连接前删除残留的套接字。即使旧 SSH
进程仍然存在，已取消链接的套接字也无法访问，新连接会重新创建该路径。

```sh
ssh your-server 'rm -f /tmp/herdr-remote-download-your-name.sock'
herdr --remote your-server-herdr --remote-keybindings server
```

如果希望继续使用 `hr your-server`执行上述两个命令，请将以下函数添加到 Mac
的 `~/.zshrc`。请将 `your-server`和 `your-name`替换为上面使用的相同值。

```zsh
unalias hr 2>/dev/null
hr() {
  if [[ "$1" == "your-server" ]]; then
    shift
    command ssh your-server \
      'rm -f /tmp/herdr-remote-download-your-name.sock' || return
    command herdr --remote-keybindings server --remote your-server-herdr "$@"
  else
    command herdr --remote-keybindings server --remote "$@"
  fi
}
compdef _herdr hr
```

`unalias`可确保当前 shell 中旧的 `hr`别名被此函数替换。重新加载一次配置并检查：

```sh
source ~/.zshrc
whence -w hr
# hr: function
```

## 使用方法

### 选择当前屏幕显示的文件路径

按 `prefix+d`后，当前屏幕中存在的文件路径会显示提示字符。输入提示字符即可将
对应文件传输到 Mac。按 Esc 关闭选择器。

选择器可以识别绝对路径、相对于 pane 当前工作目录的路径，以及以 `:行号`或
`:行号:列号`结尾的路径。目标不限于 Codex 或 Claude 输出的文件；当前屏幕中
显示的任何现有文件都可以选择。

也可以直接调用 action：

```sh
herdr plugin action invoke kosukeyano.remote-download.pick
```

### 从 `file://` 链接下载

在 Herdr 中按住 Control 并单击 Codex、Claude 或其他工具显示的 `file://`
链接，即可传输链接目标。在 macOS 上，Herdr 使用 Control 作为链接操作的修饰键。

### 从选中文本下载

选择一个文件路径，然后直接调用
`kosukeyano.remote-download.download` action。

## 限制

- 不能直接传输目录。请先将目录打包。
- 默认情况下会拒绝大于 512 MiB 的文件。
- 仅 macOS 支持通过 launchd 自动启动接收服务。
- 每台远程主机都需要单独配置 SSH `RemoteForward`和认证令牌。

## 故障排除

如果连接时显示 `remote port forwarding failed for listen path`，请先运行
上面所示的 `rm -f`命令，然后重新连接。

如果传输过程中 SSH 隧道断开，选择文件后三秒内会在选择器中显示失败原因。
按 Esc 或 Enter 关闭，然后重新连接 Herdr。

## 开发

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
```

文件选择器界面和提示字符分配使用
[herdr-tiny-fingers](https://github.com/hotchpotch/herdr-tiny-fingers)。
第三方许可证信息请参阅 [`LICENSES`](LICENSES)。

本项目使用 [MIT License](LICENSE) 发布。
