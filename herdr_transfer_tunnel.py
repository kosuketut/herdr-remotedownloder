#!/usr/bin/env python3
"""Keep the Mac transfer tunnel running via a per-host LaunchAgent."""

import argparse
from datetime import datetime
import os
from pathlib import Path
import plistlib
import re
import shlex
import shutil
import subprocess
import sys

# Probe on the remote host: never unlink a listening tunnel's socket.
REMOTE_PREPARE = r"""
import errno, getpass, os, socket, stat
p = '/tmp/herdr-remote-download-' + getpass.getuser() + '.sock'
s = socket.socket(socket.AF_UNIX)
s.settimeout(5)
try:
    s.connect(p)
except OSError as e:
    if e.errno == errno.ECONNREFUSED:
        info = os.lstat(p)
        if not stat.S_ISSOCK(info.st_mode) or info.st_uid != os.getuid():
            raise RuntimeError('Refusing to remove an unowned or non-socket path')
        os.unlink(p)
    elif e.errno != errno.ENOENT:
        raise
else:
    raise SystemExit('Transfer tunnel already listening; leaving it untouched')
finally:
    s.close()
"""

# Runs on the remote host for the tunnel's lifetime. Exiting ends the SSH
# session so launchd reconnects when the forwarded socket is removed or
# replaced (for example by an old hr function's rm -f), which SSH itself
# cannot detect. The parent check stops orphans after a dropped connection.
REMOTE_WATCH = r"""
import getpass, os, sys, time
p = '/tmp/herdr-remote-download-' + getpass.getuser() + '.sock'
parent = os.getppid()
for _ in range(20):
    try:
        inode = os.stat(p).st_ino
        break
    except FileNotFoundError:
        time.sleep(0.5)
else:
    sys.exit('Forwarded socket did not appear; reconnecting')
while os.getppid() == parent:
    time.sleep(10)
    try:
        if os.stat(p).st_ino != inode:
            break
    except FileNotFoundError:
        break
sys.exit('Forwarded socket was removed or replaced; reconnecting')
"""


def log(message):
    stamp = datetime.now().isoformat(sep=' ', timespec='seconds')
    print(f'{stamp} {message}', file=sys.stderr, flush=True)


def run(host):
    options = ['-o', 'BatchMode=yes', '-o', 'ConnectTimeout=10',
               '-o', 'ServerAliveInterval=15', '-o', 'ServerAliveCountMax=3']
    log(f'Connecting the transfer tunnel to {host}')
    result = subprocess.run(
        ['/usr/bin/ssh', *options, '-o', 'ClearAllForwardings=yes', host,
         'python3 -c ' + shlex.quote(REMOTE_PREPARE)])
    if result.returncode != 0:
        log(f'Remote socket check failed (exit {result.returncode}); launchd will retry')
        sys.exit(1)
    os.execv('/usr/bin/ssh', [
        'ssh', *options, '-o', 'ExitOnForwardFailure=yes', '-T', host + '-herdr',
        'exec python3 -c ' + shlex.quote(REMOTE_WATCH)])


def install(host):
    home = Path.home()
    root = home / '.local/share/herdr-remote-download'
    root.mkdir(parents=True, exist_ok=True)
    script = root / 'herdr_transfer_tunnel.py'
    if Path(__file__).resolve() != script.resolve():
        shutil.copyfile(__file__, script)
    label = 'com.kosukeyano.herdr-transfer-tunnel.' + host
    log = home / 'Library/Logs' / (label + '.log')
    log.parent.mkdir(parents=True, exist_ok=True)
    plist = home / 'Library/LaunchAgents' / (label + '.plist')
    plist.parent.mkdir(parents=True, exist_ok=True)
    data = {
        'Label': label,
        'ProgramArguments': [sys.executable, str(script), 'run', host],
        'RunAtLoad': True,
        'KeepAlive': True,
        'ThrottleInterval': 30,
        'StandardOutPath': str(log),
        'StandardErrorPath': str(log),
    }
    with plist.open('wb') as stream:
        plistlib.dump(data, stream)
    domain = f'gui/{os.getuid()}'
    subprocess.run(['launchctl', 'bootout', f'{domain}/{label}'],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(['launchctl', 'bootstrap', domain, str(plist)], check=True)
    print(f'Automatic transfer tunnel installed: {plist}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('command', choices=['install', 'run'])
    parser.add_argument('host')
    args = parser.parse_args()
    if not re.fullmatch(r'[A-Za-z0-9_][A-Za-z0-9._-]*', args.host):
        parser.error('unsafe SSH host name')
    if args.command == 'install':
        install(args.host)
    else:
        run(args.host)


if __name__ == '__main__':
    main()
