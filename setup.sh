#!/bin/sh

set -eu

PLUGIN_ID="kosukeyano.remote-download"
PLUGIN_REPOSITORY="kosuketut/herdr-remotedownloder"
TRANSFER_PORT="18340"
SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

usage() {
    cat <<'EOF'
Usage: ./setup.sh <ssh-host>

Install Herdr Remote File Transfer on this Mac and the named SSH host.
The SSH host must already work with: ssh <ssh-host>
EOF
}

fail() {
    printf 'setup: %s\n' "$*" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

backup_once() {
    source_path=$1
    backup_path="${source_path}.before-herdr-remote-download.bak"
    if [ -f "$source_path" ] && [ ! -e "$backup_path" ]; then
        cp -p "$source_path" "$backup_path"
    fi
}

file_mode() {
    stat -f '%Lp' "$1"
}

replace_file_if_changed() {
    destination=$1
    candidate=$2
    default_mode=$3

    if [ -f "$destination" ] && cmp -s "$destination" "$candidate"; then
        return
    fi
    if [ -f "$destination" ]; then
        mode=$(file_mode "$destination")
        backup_once "$destination"
    else
        mode=$default_mode
    fi
    chmod "$mode" "$candidate"
    mv "$candidate" "$destination"
}

replace_managed_block() {
    source_path=$1
    block_path=$2
    begin_marker=$3
    end_marker=$4
    output_path=$5

    markers=$(awk -v begin="$begin_marker" -v end="$end_marker" '
        $0 == begin { begins += 1 }
        $0 == end { ends += 1 }
        END { print begins + 0, ends + 0 }
    ' "$source_path")
    set -- $markers
    [ "$1" -eq "$2" ] && [ "$1" -le 1 ] ||
        fail "cannot safely update malformed managed block in $source_path"

    awk -v begin="$begin_marker" -v end="$end_marker" '
        $0 == begin { skipping = 1; next }
        skipping && $0 == end { skipping = 0; next }
        !skipping && $0 == "" { trailing_blanks = trailing_blanks "\n"; next }
        !skipping {
            printf "%s", trailing_blanks
            trailing_blanks = ""
            print
        }
    ' "$source_path" > "$output_path"

    if [ -s "$output_path" ]; then
        printf '\n' >> "$output_path"
    fi
    cat "$block_path" >> "$output_path"
}

write_ssh_host_block() {
    effective_config=$1
    block_path=$2
    ssh_target=$3
    herdr_target=$4
    remote_user=$5
    begin_marker=$6
    end_marker=$7

    {
        printf '%s\n' "$begin_marker"
        printf 'Host %s\n' "$herdr_target"
        while IFS= read -r line; do
            key=${line%% *}
            value=${line#* }
            case "$key" in
                hostname) printf '    HostName %s\n' "$value" ;;
                user) printf '    User %s\n' "$value" ;;
                port) printf '    Port %s\n' "$value" ;;
                identityfile) printf '    IdentityFile %s\n' "$value" ;;
                certificatefile)
                    [ "$value" = "none" ] || printf '    CertificateFile %s\n' "$value"
                    ;;
                identityagent)
                    [ "$value" = "none" ] || printf '    IdentityAgent %s\n' "$value"
                    ;;
                identitiesonly) printf '    IdentitiesOnly %s\n' "$value" ;;
                usekeychain) printf '    UseKeychain %s\n' "$value" ;;
                batchmode) printf '    BatchMode %s\n' "$value" ;;
                preferredauthentications)
                    [ "$value" = "none" ] || printf '    PreferredAuthentications %s\n' "$value"
                    ;;
                pubkeyauthentication) printf '    PubkeyAuthentication %s\n' "$value" ;;
                passwordauthentication) printf '    PasswordAuthentication %s\n' "$value" ;;
                kbdinteractiveauthentication)
                    printf '    KbdInteractiveAuthentication %s\n' "$value"
                    ;;
                proxyjump)
                    [ "$value" = "none" ] || printf '    ProxyJump %s\n' "$value"
                    ;;
                proxycommand)
                    [ "$value" = "none" ] || printf '    ProxyCommand %s\n' "$value"
                    ;;
                proxyusefdpass) printf '    ProxyUseFdpass %s\n' "$value" ;;
                hostkeyalias)
                    [ "$value" = "none" ] || printf '    HostKeyAlias %s\n' "$value"
                    ;;
                userknownhostsfile) printf '    UserKnownHostsFile %s\n' "$value" ;;
                stricthostkeychecking) printf '    StrictHostKeyChecking %s\n' "$value" ;;
                addkeystoagent) printf '    AddKeysToAgent %s\n' "$value" ;;
                forwardagent) printf '    ForwardAgent %s\n' "$value" ;;
            esac
        done < "$effective_config"
        printf '    RemoteForward /tmp/herdr-remote-download-%s.sock 127.0.0.1:%s\n' \
            "$remote_user" "$TRANSFER_PORT"
        printf '    StreamLocalBindUnlink yes\n'
        printf '    ExitOnForwardFailure yes\n'
        printf '    ServerAliveInterval 15\n'
        printf '    ServerAliveCountMax 3\n'
        printf '    ControlMaster no\n'
        printf '    ControlPath none\n'
        printf '%s\n' "$end_marker"
    } > "$block_path"
}

configure_ssh() {
    ssh_target=$1
    herdr_target=$2
    remote_user=$3
    effective_config=$4
    temp_dir=$5

    ssh_dir="$HOME/.ssh"
    ssh_config="$ssh_dir/config"
    managed_config="$ssh_dir/.herdr-remote-download.config"
    include_line='Include ~/.ssh/.herdr-remote-download.config'
    begin_marker="# BEGIN herdr-remote-download: $ssh_target"
    end_marker="# END herdr-remote-download: $ssh_target"

    mkdir -p "$ssh_dir"
    chmod 700 "$ssh_dir"
    if [ ! -e "$ssh_config" ]; then
        : > "$ssh_config"
        chmod 600 "$ssh_config"
    fi
    if [ ! -e "$managed_config" ]; then
        : > "$managed_config"
        chmod 600 "$managed_config"
    fi

    ssh_block="$temp_dir/ssh-block"
    managed_candidate="$temp_dir/managed-config"
    main_candidate="$temp_dir/ssh-config"
    write_ssh_host_block "$effective_config" "$ssh_block" "$ssh_target" \
        "$herdr_target" "$remote_user" "$begin_marker" "$end_marker"
    replace_managed_block "$managed_config" "$ssh_block" "$begin_marker" \
        "$end_marker" "$managed_candidate"
    replace_file_if_changed "$managed_config" "$managed_candidate" 600

    {
        printf '%s\n\n' "$include_line"
        awk -v include="$include_line" '
            $0 == include { removed = 1; next }
            removed && $0 == "" { removed = 0; next }
            { removed = 0; print }
        ' "$ssh_config"
    } > "$main_candidate"
    replace_file_if_changed "$ssh_config" "$main_candidate" 600

    ssh -G "$herdr_target" > "$temp_dir/herdr-effective-ssh"
    grep -F "remoteforward /tmp/herdr-remote-download-${remote_user}.sock " \
        "$temp_dir/herdr-effective-ssh" >/dev/null ||
        fail "generated SSH forwarding configuration was not accepted"
}

configure_zsh() {
    temp_dir=$1
    zshrc="$HOME/.zshrc"
    begin_marker='# BEGIN herdr-remote-download setup'
    end_marker='# END herdr-remote-download setup'
    block="$temp_dir/zsh-block"
    candidate="$temp_dir/zshrc"

    if [ ! -e "$zshrc" ]; then
        : > "$zshrc"
        chmod 600 "$zshrc"
    fi
    cat > "$block" <<'EOF'
# BEGIN herdr-remote-download setup
unalias hr 2>/dev/null
hr() {
  if (( $# == 0 )); then
    command herdr --remote-keybindings server --remote
    return
  fi

  local target="$1"
  shift
  local managed_config="${HOME}/.ssh/.herdr-remote-download.config"
  if [[ -f "$managed_config" ]] &&
      command grep -Fq "# BEGIN herdr-remote-download: ${target}" "$managed_config"; then
    local remote_user
    remote_user=$(command ssh -G "$target" 2>/dev/null |
      command awk '$1 == "user" { print $2; exit }')
    case "$remote_user" in
      ""|*[!A-Za-z0-9_.-]*)
        print -u2 "hr: could not determine a safe remote user for ${target}"
        return 1
        ;;
    esac
    command ssh "$target" \
      "rm -f /tmp/herdr-remote-download-${remote_user}.sock" || return
    command herdr --remote-keybindings server --remote "${target}-herdr" "$@"
  else
    command herdr --remote-keybindings server --remote "$target" "$@"
  fi
}
compdef _herdr hr
# END herdr-remote-download setup
EOF

    replace_managed_block "$zshrc" "$block" "$begin_marker" "$end_marker" "$candidate"
    replace_file_if_changed "$zshrc" "$candidate" 600
}

case ${1:-} in
    -h|--help)
        usage
        exit 0
        ;;
esac
[ "$#" -eq 1 ] || {
    usage >&2
    exit 2
}

SSH_TARGET=$1
case "$SSH_TARGET" in
    -*|*[!A-Za-z0-9._-]*)
        fail "SSH host must use only letters, numbers, '.', '_' or '-'"
        ;;
esac
HERDR_TARGET="${SSH_TARGET}-herdr"

[ "$(uname -s)" = "Darwin" ] || fail "the automatic setup currently requires macOS"
for command_name in cargo curl git herdr ssh; do
    require_command "$command_name"
done
[ -f "$SCRIPT_DIR/Cargo.lock" ] || fail "run setup.sh from a complete repository checkout"

TEMP_DIR=$(mktemp -d "${TMPDIR:-/tmp}/herdr-remote-download-setup.XXXXXX")
cleanup() {
    rm -rf "$TEMP_DIR"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

printf '[1/7] Checking the local and remote requirements...\n'
herdr --version
ssh -G "$SSH_TARGET" > "$TEMP_DIR/effective-ssh"
REMOTE_USER=$(ssh "$SSH_TARGET" '
    set -eu
    for command_name in cargo git herdr python3; do
        command -v "$command_name" >/dev/null 2>&1 || {
            printf "required command not found on the remote host: %s\n" "$command_name" >&2
            exit 1
        }
    done
    id -un
')
case "$REMOTE_USER" in
    ''|*[!A-Za-z0-9_.-]*) fail "the remote host returned an unsafe user name" ;;
esac

printf '[2/7] Building and starting the Mac transfer service...\n'
cd "$SCRIPT_DIR"
cargo build --release --locked
./target/release/herdr-remote-download install-service
./target/release/herdr-remote-download service-status
curl --fail --silent --show-error --retry 10 --retry-connrefused --retry-delay 1 \
    --max-time 2 "http://127.0.0.1:${TRANSFER_PORT}/health" >/dev/null

printf '[3/7] Installing the plugin on %s...\n' "$SSH_TARGET"
ssh "$SSH_TARGET" herdr plugin install --yes "$PLUGIN_REPOSITORY"

printf '[4/7] Copying the authentication token...\n'
ssh "$SSH_TARGET" '
    set -eu
    config_dir=$(herdr plugin config-dir kosukeyano.remote-download)
    mkdir -p "$config_dir"
    umask 077
    cat > "$config_dir/token"
' < "$HOME/.config/herdr-remote-download/token"

printf '[5/7] Configuring the remote keybindings...\n'
ssh "$SSH_TARGET" /bin/sh -s -- "$PLUGIN_ID" <<'REMOTE_SETUP'
set -eu
plugin_id=$1
plugin_root=$(
    herdr plugin list --plugin "$plugin_id" --json |
        python3 -c 'import json, sys; plugins = json.load(sys.stdin)["result"]["plugins"]; sys.exit("installed plugin not found") if len(plugins) != 1 else print(plugins[0]["plugin_root"])'
)
config_path=${HERDR_CONFIG_PATH:-"$HOME/.config/herdr/config.toml"}
mkdir -p "$(dirname "$config_path")"
if [ ! -e "$config_path" ]; then
    : > "$config_path"
    chmod 600 "$config_path"
fi
"$plugin_root/target/release/herdr-remote-download" configure-keybinding \
    --config "$config_path"
python3 "$plugin_root/herdr_remote_upload.py" configure-upload-keybinding \
    --config "$config_path"
herdr config check
if herdr status server >/dev/null 2>&1; then
    herdr server reload-config
else
    printf 'Herdr server is not running; the keybindings will load on its next start.\n'
fi
REMOTE_SETUP

printf '[6/7] Configuring the SSH forwarding host...\n'
configure_ssh "$SSH_TARGET" "$HERDR_TARGET" "$REMOTE_USER" \
    "$TEMP_DIR/effective-ssh" "$TEMP_DIR"

printf '[7/7] Configuring the hr command...\n'
configure_zsh "$TEMP_DIR"

printf '\nSetup complete. Restart the shell or run:\n\n'
printf '  source ~/.zshrc\n'
printf '  hr %s\n\n' "$SSH_TARGET"
