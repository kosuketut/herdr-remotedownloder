import errno
import importlib.util
import io
from pathlib import Path
import stat
import types
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location(
    'tunnel', Path(__file__).resolve().parents[1] / 'herdr_transfer_tunnel.py')
tunnel = importlib.util.module_from_spec(spec)
spec.loader.exec_module(tunnel)


class TunnelTests(unittest.TestCase):
    def test_prepare_only_removes_stale_owned_socket(self):
        for error, removed in [(None, False),
                               (ConnectionRefusedError(errno.ECONNREFUSED, 'stale'), True),
                               (FileNotFoundError(errno.ENOENT, 'missing'), False),
                               (TimeoutError('timeout'), False)]:
            with self.subTest(error=error), patch('socket.socket') as sock, \
                    patch('os.unlink') as unlink, patch('os.lstat') as lstat:
                sock.return_value.connect.side_effect = error
                lstat.return_value = types.SimpleNamespace(
                    st_mode=stat.S_IFSOCK, st_uid=tunnel.os.getuid())
                if error is None:
                    with self.assertRaises(SystemExit):
                        exec(tunnel.REMOTE_PREPARE, {})
                elif isinstance(error, TimeoutError):
                    with self.assertRaises(TimeoutError):
                        exec(tunnel.REMOTE_PREPARE, {})
                else:
                    exec(tunnel.REMOTE_PREPARE, {})
                self.assertEqual(unlink.called, removed)
                sock.return_value.close.assert_called_once()

    def test_watch_exits_when_socket_disappears_or_is_replaced(self):
        def stat_results(*inodes):
            def fake_stat(path):
                inode = next(results)
                if inode is None:
                    raise FileNotFoundError(errno.ENOENT, 'missing', path)
                return types.SimpleNamespace(st_ino=inode)
            results = iter(inodes)
            return fake_stat

        for name, inodes, parents, message in [
                ('removed', (1, 1, None), (7, 7, 7, 7), 'removed or replaced'),
                ('replaced', (1, 1, 2), (7, 7, 7, 7), 'removed or replaced'),
                ('orphaned', (1, 1), (7, 7, 1), 'removed or replaced'),
                ('never bound', (None,) * 20, (7,), 'did not appear')]:
            with self.subTest(name), \
                    patch('os.stat', side_effect=stat_results(*inodes)), \
                    patch('os.getppid', side_effect=parents), \
                    patch('time.sleep'):
                with self.assertRaises(SystemExit) as raised:
                    exec(tunnel.REMOTE_WATCH, {})
                self.assertIn(message, str(raised.exception.code))

    def test_run_prepares_without_forwarding_then_executes_watched_tunnel(self):
        with patch.object(tunnel.subprocess, 'run') as run, \
                patch.object(tunnel.os, 'execv') as execute, \
                patch('sys.stderr', io.StringIO()):
            run.return_value.returncode = 0
            tunnel.run('mercury')
            self.assertIn('ClearAllForwardings=yes', run.call_args.args[0])
            arguments = execute.call_args.args[1]
            self.assertIn('ExitOnForwardFailure=yes', arguments)
            self.assertEqual(arguments[-2], 'mercury-herdr')
            self.assertTrue(arguments[-1].startswith('exec python3 -c '))
        log = io.StringIO()
        with patch.object(tunnel.subprocess, 'run') as run, \
                patch.object(tunnel.os, 'execv') as execute, \
                patch('sys.stderr', log):
            run.return_value.returncode = 255
            with self.assertRaises(SystemExit):
                tunnel.run('mercury')
            execute.assert_not_called()
        self.assertRegex(log.getvalue(), r'^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d Connecting')
        self.assertIn('failed (exit 255)', log.getvalue())


if __name__ == '__main__':
    unittest.main()
