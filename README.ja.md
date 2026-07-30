# Herdr Remote File Download

[English](README.md) | 日本語 | [简体中文](README.zh-CN.md)

`herdr --remote` の接続先にあるファイルを、接続元Macの `~/Downloads` へ
ダウンロードするHerdrプラグインです。

表示中のファイルパスをヒント文字で選択するほか、CodexやClaudeなどが表示した
`file://` リンクからも転送できます。PDF、PPTX、画像、アーカイブなど、
拡張子に関係なく通常のファイルを転送します。

## 仕組み

Herdrのプラグインは接続先で動作するため、接続元Macで小さな受信サービスを
起動します。転送には公開ポートを使用せず、SSH `RemoteForward`の
ループバック接続だけを使用します。

```text
remote Herdr plugin
  -> /tmp/herdr-remote-download-<remote-user>.sock
  -> SSH RemoteForward
  -> 127.0.0.1:18340 on the connected Mac
  -> ~/Downloads
```

転送は64文字のランダムトークンで認証し、受信後にSHA-256を検証します。
既定の上限は512 MiBです。同名ファイルがある場合は `name (1).ext` のように
保存し、既存ファイルを上書きしません。

## 必要環境

- Herdr 0.7.0以降
- 接続元: macOS、Python 3.9以降
- 接続先: LinuxまたはmacOS、Python 3.9以降、Git、Rust/Cargo 1.88以降
- SSH configで指定できる接続先

## セットアップ

以下ではSSH configの接続先名を `your-server` とします。

### 1. Mac側の受信サービス

Macでリポジトリを取得し、launchdサービスをインストールします。

```sh
git clone https://github.com/kosuketut/herdr-remotedownloder.git
cd herdr-remotedownloder
python3 herdr_remote_download.py install-service
python3 herdr_remote_download.py service-status
curl -fsS http://127.0.0.1:18340/health
```

受信サービスは `127.0.0.1` のみにbindし、ログを
`~/Library/Logs/herdr-remote-download.log`へ書きます。認証トークンは
`~/.config/herdr-remote-download/token`に作成されます。

### 2. SSHトンネル

Macの `~/.ssh/config` にHerdr専用のHostを追加します。通常のSSH接続と
転送ポートを分離するため、既存のHostとは別名にしてください。このブロックは
`Host *`より前に置きます。

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

ソケット名の `your-name` は接続先で `id -un`が表示するユーザー名に合わせます。
Unixソケットを使うことで、古いSSHセッションと新しいセッションが同じTCPポートを
共有する問題を避けます。`ExitOnForwardFailure yes`はソケットを作成できない場合、
転送不能のままHerdrを起動せず、接続時点でエラーにします。

### 3. 接続先へプラグインをインストール

接続先で実行します。

```sh
herdr plugin install kosuketut/herdr-remotedownloder
```

インストーラーは内容の確認後、マニフェストに従ってRust製pickerをビルドします。

### 4. 認証トークンを接続先へコピー

Macで次を実行します。

```sh
ssh your-server \
  'config_dir=$(herdr plugin config-dir kosukeyano.remote-download) &&
   mkdir -p "$config_dir" &&
   umask 077 &&
   cat > "$config_dir/token"' \
  < ~/.config/herdr-remote-download/token
```

トークンをリポジトリへ追加したり、第三者へ共有したりしないでください。

### 5. キーバインド

接続先の `~/.config/herdr/config.toml`へ次を追加します。

```toml
[[keys.command]]
key = "prefix+d"
type = "plugin_action"
command = "kosukeyano.remote-download.pick"
description = "pick a remote file to download"
```

設定を反映します。

```sh
herdr server reload-config
```

server側キーバインドをremote接続で使用するには、次のように接続します。
接続前に残存ソケットを削除してください。古いSSHプロセスが残っていても、
削除済みソケットは到達不能になり、新しい接続だけが同じパスを作成します。

```sh
ssh your-server 'rm -f /tmp/herdr-remote-download-your-name.sock'
herdr --remote your-server-herdr --remote-keybindings server
```

この2コマンドを従来どおり `hr your-server`で実行する場合は、Macの
`~/.zshrc`へ次を追加します。`your-server`と `your-name`は上の設定と
同じ値へ置き換えてください。

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

`unalias`は、現在のシェルに古い `hr`エイリアスが残っている場合も、関数へ
確実に置き換えるために必要です。設定後、一度だけ再読込して確認します。

```sh
source ~/.zshrc
whence -w hr
# hr: function
```

## 使い方

### 表示中のファイルパスを選ぶ

`prefix+d`を押すと、表示中にある実在ファイルのパスへヒント文字が付きます。
ヒント文字を入力すると、そのファイルをMacへ転送します。Escで閉じます。

絶対パス、paneのcwdからの相対パス、`:行`、`:行:列`付きのパスを認識します。
CodexやClaudeの出力に限らず、現在表示されている実在ファイルが対象です。

キーバインドを使わず、actionを直接起動することもできます。

```sh
herdr plugin action invoke kosukeyano.remote-download.pick
```

### `file://` リンクからダウンロード

CodexやClaudeなどが表示した `file://` リンクをHerdr上でControl+クリックすると、
リンク先のファイルを転送します。macOSでもHerdrがリンク操作として捕捉する
修飾キーはControlです。

### 選択テキストからダウンロード

ファイルパスを選択して、`kosukeyano.remote-download.download` actionを
直接呼び出すこともできます。

## 制限事項

- ディレクトリは転送できません。アーカイブしてから選択してください。
- 既定では512 MiBを超えるファイルを転送できません。
- launchdによる受信サービスの自動起動はmacOSのみ対応しています。
- SSH `RemoteForward`と認証トークンの設定は、接続先ごとに必要です。

## トラブルシューティング

接続時に `remote port forwarding failed for listen path` と表示された場合は、
上記の `rm -f`を実行してから再接続してください。
転送中にSSHトンネルが失われた場合は、ファイル選択後3秒以内にpickerへ失敗理由を
表示します。EscまたはEnterで閉じ、Herdrを再接続してください。

## 開発

```sh
python3 -m unittest discover -s tests -v
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --release --locked
```

pickerの表示とヒント割当には
[herdr-tiny-fingers](https://github.com/hotchpotch/herdr-tiny-fingers)
を利用しています。第三者ライセンスは
[`LICENSES`](LICENSES)を参照してください。

このプロジェクトは[MIT License](LICENSE)で公開されています。
