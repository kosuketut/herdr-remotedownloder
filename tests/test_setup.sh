#!/bin/sh

set -eu

PROJECT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/herdr-setup-test.XXXXXX")
cleanup() {
    rm -rf "$TEST_ROOT"
}
trap cleanup EXIT HUP INT TERM

TEST_REPOSITORY="$TEST_ROOT/repository"
TEST_HOME="$TEST_ROOT/home"
FAKE_BIN="$TEST_ROOT/bin"
SETUP_TEST_LOG="$TEST_ROOT/commands.log"
mkdir -p "$TEST_REPOSITORY" "$TEST_HOME/.ssh" "$FAKE_BIN"
cp "$PROJECT_DIR/setup.sh" "$TEST_REPOSITORY/setup.sh"
: > "$TEST_REPOSITORY/Cargo.lock"

cat > "$FAKE_BIN/uname" <<'EOF'
#!/bin/sh
printf 'Darwin\n'
EOF

cat > "$FAKE_BIN/herdr" <<'EOF'
#!/bin/sh
if [ "${1:-}" = "--version" ]; then
    printf 'herdr 0.7.5\n'
    exit 0
fi
exit 0
EOF

cat > "$FAKE_BIN/cargo" <<'EOF'
#!/bin/sh
mkdir -p target/release
cat > target/release/herdr-remote-download <<'SERVICE'
#!/bin/sh
case ${1:-} in
    install-service)
        mkdir -p "$HOME/.config/herdr-remote-download"
        printf '%064d\n' 0 > "$HOME/.config/herdr-remote-download/token"
        printf '%s\n' "$HOME/Library/LaunchAgents/dev.herdr.remote-download.plist"
        ;;
    service-status)
        printf '{"running":true}\n'
        ;;
    *)
        exit 1
        ;;
esac
SERVICE
chmod +x target/release/herdr-remote-download
EOF

cat > "$FAKE_BIN/curl" <<'EOF'
#!/bin/sh
exit 0
EOF

cat > "$FAKE_BIN/ssh" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$SETUP_TEST_LOG"
if [ "${1:-}" = "-G" ]; then
    host=$2
    cat <<CONFIG
user kyano
hostname remote.example.com
port 2222
batchmode no
forwardagent no
identitiesonly yes
identityfile ~/.ssh/id_test
proxyjump jump.example.com
proxyusefdpass no
stricthostkeychecking ask
userknownhostsfile ~/.ssh/known_hosts
addkeystoagent true
pubkeyauthentication true
passwordauthentication yes
kbdinteractiveauthentication yes
CONFIG
    case "$host" in
        *-herdr)
            printf 'remoteforward /tmp/herdr-remote-download-kyano.sock [127.0.0.1]:18340\n'
            ;;
    esac
    exit 0
fi

shift
case "$*" in
    *"id -un"*)
        printf 'kyano\n'
        ;;
    *"cat >"*)
        cat >/dev/null
        ;;
    "/bin/sh -s -- "*)
        cat >/dev/null
        ;;
esac
exit 0
EOF

chmod +x "$FAKE_BIN/uname" "$FAKE_BIN/herdr" "$FAKE_BIN/cargo" \
    "$FAKE_BIN/curl" "$FAKE_BIN/ssh"

cat > "$TEST_HOME/.ssh/config" <<'EOF'
Host mercury
    HostName original.example.com
    User kyano
EOF
cat > "$TEST_HOME/.zshrc" <<'EOF'
export KEEP_THIS_SETTING=1
EOF
cp "$TEST_HOME/.ssh/config" "$TEST_ROOT/original-ssh-config"
cp "$TEST_HOME/.zshrc" "$TEST_ROOT/original-zshrc"

run_setup() {
    HOME="$TEST_HOME" \
    PATH="$FAKE_BIN:/usr/bin:/bin" \
    SETUP_TEST_LOG="$SETUP_TEST_LOG" \
        "$TEST_REPOSITORY/setup.sh" "$1" >/dev/null
}

run_setup mercury

SSH_CONFIG="$TEST_HOME/.ssh/config"
MANAGED_CONFIG="$TEST_HOME/.ssh/.herdr-remote-download.config"
ZSHRC="$TEST_HOME/.zshrc"

[ "$(grep -Fxc 'Include ~/.ssh/.herdr-remote-download.config' "$SSH_CONFIG")" -eq 1 ]
grep -F 'Host mercury' "$SSH_CONFIG" >/dev/null
grep -F 'Host mercury-herdr' "$MANAGED_CONFIG" >/dev/null
grep -F '    ProxyJump jump.example.com' "$MANAGED_CONFIG" >/dev/null
grep -F '    RemoteForward /tmp/herdr-remote-download-kyano.sock 127.0.0.1:18340' \
    "$MANAGED_CONFIG" >/dev/null
grep -F '    StreamLocalBindUnlink yes' "$MANAGED_CONFIG" >/dev/null
/usr/bin/ssh -F "$MANAGED_CONFIG" -G mercury-herdr 2>/dev/null |
    grep -F 'remoteforward /tmp/herdr-remote-download-kyano.sock ' >/dev/null
grep -F 'export KEEP_THIS_SETTING=1' "$ZSHRC" >/dev/null
[ "$(grep -Fxc '# BEGIN herdr-remote-download setup' "$ZSHRC")" -eq 1 ]
cmp "$TEST_ROOT/original-ssh-config" \
    "$SSH_CONFIG.before-herdr-remote-download.bak"
cmp "$TEST_ROOT/original-zshrc" "$ZSHRC.before-herdr-remote-download.bak"
grep -F 'herdr plugin install --yes kosuketut/herdr-remotedownloder' \
    "$SETUP_TEST_LOG" >/dev/null

SSH_CHECKSUM=$(cksum "$SSH_CONFIG" "$MANAGED_CONFIG" "$ZSHRC")
run_setup mercury
[ "$SSH_CHECKSUM" = "$(cksum "$SSH_CONFIG" "$MANAGED_CONFIG" "$ZSHRC")" ]
[ "$(grep -Fxc '# BEGIN herdr-remote-download: mercury' "$MANAGED_CONFIG")" -eq 1 ]
[ "$(grep -Fxc '# BEGIN herdr-remote-download setup' "$ZSHRC")" -eq 1 ]

run_setup earth
[ "$(grep -c '^Host .*\-herdr$' "$MANAGED_CONFIG")" -eq 2 ]
[ "$(grep -Fxc '# BEGIN herdr-remote-download setup' "$ZSHRC")" -eq 1 ]

sh -n "$TEST_REPOSITORY/setup.sh"
zsh -n "$ZSHRC"
printf 'setup tests passed\n'
