#!/usr/bin/env python3
"""Isolated deployment tests: no user creation, live systemctl, MTA or install."""
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

PROJECT = Path(__file__).resolve().parents[1]
GENERATOR = PROJECT / 'deploy/fim_page_generator.py'
SUPERVISOR_SPEC = importlib.util.spec_from_file_location('fim_failure_mail', PROJECT / 'deploy/failure_mail.py')
SUPERVISOR = importlib.util.module_from_spec(SUPERVISOR_SPEC)
SUPERVISOR_SPEC.loader.exec_module(SUPERVISOR)


class DeploymentTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix='fim-deploy-')
        self.root = Path(self.temporary.name)
        self.config = self.root / 'config.json'
        self.config.write_text(json.dumps({'timezone': 'Europe/Kyiv'}))
        self.output = self.root / 'generated'

    def tearDown(self):
        self.temporary.cleanup()

    def generate(self, expected=0):
        result = subprocess.run([sys.executable, str(GENERATOR), '--config', str(self.config), str(self.output)],
                                text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, expected, result.stderr)
        return (self.output / 'fim-page.timer.d/20-timezone.conf').read_text()

    def test_schedule_matches_config_and_later_edits(self):
        self.assertEqual(self.generate(), '[Timer]\nOnCalendar=\nOnCalendar=*-*-* 00:00:00 Europe/Kyiv\n')
        self.config.write_text(json.dumps({'timezone': 'Europe/Bratislava'}))
        self.assertEqual(self.generate(), '[Timer]\nOnCalendar=\nOnCalendar=*-*-* 00:00:00 Europe/Bratislava\n')
        watcher = (PROJECT / 'deploy/fim-config.path').read_text()
        self.assertIn('PathChanged=/opt/fim/config.json', watcher)
        reload_unit = (PROJECT / 'deploy/fim-config.service').read_text()
        self.assertIn('ExecStart=/usr/bin/systemctl daemon-reload', reload_unit)
        self.assertIn('ExecStart=/usr/bin/systemctl restart fim-page.timer', reload_unit)

    def test_invalid_timezone_removes_previous_schedule(self):
        for invalid in ('Invalid/Nowhere', 'Europe/Kyiv\nOnCalendar=minutely', '../UTC',
                        '/etc/passwd', 'UTC;touch /tmp/fim-injection', '%n', '', None):
            with self.subTest(timezone=invalid):
                self.config.write_text(json.dumps({'timezone': 'Europe/Kyiv'}))
                self.generate()
                self.config.write_text(json.dumps({'timezone': invalid}))
                self.assertEqual(self.generate(1), '[Timer]\nOnCalendar=\n')

    def test_invalid_json_and_missing_config_fail_closed(self):
        self.generate()
        self.config.write_text('{')
        self.assertEqual(self.generate(1), '[Timer]\nOnCalendar=\n')
        self.config.unlink()
        self.assertEqual(self.generate(1), '[Timer]\nOnCalendar=\n')

    def test_check_config_does_not_write_files(self):
        before = set(self.root.iterdir())
        result = subprocess.run([sys.executable, str(GENERATOR), '--check-config', str(self.config)],
                                text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), 'Europe/Kyiv')
        self.assertEqual(set(self.root.iterdir()), before)

    def installer_fixture(self, case='default'):
        fixture = self.root / case
        project = fixture / 'checkout'
        (project / 'target/release').mkdir(parents=True)
        shutil.copytree(PROJECT / 'deploy', project / 'deploy')
        (project / 'site').mkdir()
        shutil.copy2(PROJECT / 'site/style.css', project / 'site/style.css')
        for name in ('config.json', 'namedays.json', 'index.template.html'):
            shutil.copy2(PROJECT / name, project / name)
        binary = project / 'target/release/fim'
        binary.write_text('#!/bin/sh\nexit 0\n')
        binary.chmod(0o755)
        # Redirect every installation destination into an isolated tree before
        # executing the real preflight. Mutating commands are additionally inert.
        script = project / 'deploy/install.sh'
        text = script.read_text()
        for prefix in ('/opt/fim', '/var/lib/fim', '/var/www/fim', '/etc/systemd'):
            text = text.replace(prefix, str(fixture / 'system' / prefix.lstrip('/')))
        script.write_text(text)
        shims = fixture / 'commands'
        shims.mkdir()
        identity = shims / 'id'
        identity.write_text('#!/bin/sh\n[ "$1" = -u ] && { echo 0; exit 0; }; exit 1\n')
        identity.chmod(0o755)
        for name in ('useradd', 'install', 'systemctl'):
            shim = shims / name
            shim.write_text('#!/bin/sh\nprintf "%s\\n" mutation >> "$FIM_MUTATION_LOG"\nexit 99\n')
            shim.chmod(0o755)
        env = os.environ.copy()
        env['PATH'] = str(shims) + os.pathsep + env['PATH']
        env['FIM_MUTATION_LOG'] = str(self.root / 'mutations')
        return project, env

    def test_installer_refuses_existing_and_broken_symlink_targets_before_mutation(self):
        targets = (
            '/opt/fim', '/var/lib/fim', '/var/www/fim',
            '/etc/systemd/system-generators/fim-page-generator',
            '/etc/systemd/system/fim.service',
            '/etc/systemd/system/fim-page.timer',
            '/etc/systemd/system/fim-config.path',
        )
        for index, (target, kind) in enumerate((target, kind) for target in targets
                                               for kind in ('existing', 'broken-link')):
            with self.subTest(target=target, kind=kind):
                project, env = self.installer_fixture(f'collision-{index}')
                destination = project.parent / 'system' / target.lstrip('/')
                destination.parent.mkdir(parents=True, exist_ok=True)
                if kind == 'broken-link':
                    destination.symlink_to(destination.parent / 'missing-original')
                elif target in ('/opt/fim', '/var/lib/fim', '/var/www/fim'):
                    destination.mkdir()
                    # Neither old config.json nor index.html exists: even a
                    # partial installation must not have its files overwritten.
                    (destination / 'keep.txt').write_text('existing data')
                else:
                    destination.write_text('existing configuration')
                result = subprocess.run(['/bin/sh', str(project / 'deploy/install.sh')], env=env,
                                        text=True, capture_output=True, timeout=10)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('Existing install target:', result.stderr)
                self.assertIn(str(destination), result.stderr)
                self.assertFalse((self.root / 'mutations').exists())
                if kind == 'broken-link':
                    self.assertTrue(destination.is_symlink())
                elif destination.is_dir():
                    self.assertEqual((destination / 'keep.txt').read_text(), 'existing data')
                else:
                    self.assertEqual(destination.read_text(), 'existing configuration')

    def test_supervisor_resolves_sendmail_relative_to_config_directory(self):
        directory = self.root / 'mail transport'
        directory.mkdir()
        executable = directory / 'sendmail-test'
        message = self.root / 'submitted.eml'
        arguments = self.root / 'arguments.json'
        executable.write_text(f'#!{sys.executable}\n'
                              'import json, sys\nfrom pathlib import Path\n'
                              f'Path({str(message)!r}).write_bytes(sys.stdin.buffer.read())\n'
                              f'Path({str(arguments)!r}).write_text(json.dumps(sys.argv[1:]))\n')
        executable.chmod(0o700)
        alias = directory / 'sendmail-alias'
        alias.symlink_to(executable.name)
        for transport in ('mail transport/sendmail-test', 'mail transport/sendmail-alias', str(executable)):
            with self.subTest(sendmail=transport):
                self.config.write_text(json.dumps({'sendmail': transport,
                                                   'mail_from': 'fim@example.com',
                                                   'mail_to': 'admin@example.com'}))
                SUPERVISOR.notify_failure(self.config, 'fim.service')
                self.assertEqual(json.loads(arguments.read_text()), ['-i', '-t'])
                self.assertIn('FIM service failed: fim.service', message.read_text())
                self.assertIn('Subject: FIM SERVICE FAILURE', message.read_text())

    def test_supervisor_rejects_oversized_configuration_before_submission(self):
        self.config.write_bytes(b' ' * (1024 * 1024 + 1))
        with self.assertRaisesRegex(ValueError, 'Configuration exceeds 1 MiB'):
            SUPERVISOR.notify_failure(self.config, 'fim.service')

    def test_installer_missing_style_fails_before_any_mutation(self):
        project, env = self.installer_fixture()
        (project / 'site/style.css').unlink()
        result = subprocess.run(['/bin/sh', str(project / 'deploy/install.sh')], env=env,
                                text=True, capture_output=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('site/style.css', result.stderr)
        self.assertFalse((self.root / 'mutations').exists())

    def test_installer_invalid_timezone_fails_before_any_mutation(self):
        project, env = self.installer_fixture()
        config = json.loads((project / 'config.json').read_text())
        config['timezone'] = 'Invalid/Nowhere'
        (project / 'config.json').write_text(json.dumps(config))
        result = subprocess.run(['/bin/sh', str(project / 'deploy/install.sh')], env=env,
                                text=True, capture_output=True, timeout=10)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('Unknown IANA timezone', result.stderr)
        self.assertFalse((self.root / 'mutations').exists())

    @unittest.skipUnless(shutil.which('systemd-analyze'), 'systemd-analyze unavailable')
    def test_generated_calendar_and_units_pass_static_systemd_checks(self):
        root = self.root / 'system'
        units = root / 'etc/systemd/system'
        units.mkdir(parents=True)
        for source in (PROJECT / 'deploy').iterdir():
            if source.suffix in ('.service', '.timer', '.path'):
                shutil.copy2(source, units / source.name)
        self.output = units
        self.generate()
        # Supply inert executable fixtures and minimal dependency targets to the
        # offline verifier. Nothing is installed into or started on the host.
        for name in ('sysinit', 'basic', 'shutdown', 'timers', 'paths', 'sockets', 'multi-user'):
            (units / f'{name}.target').write_text('[Unit]\nDefaultDependencies=no\n')
        for name in ('opt/fim/fim', 'usr/bin/python3', 'usr/bin/systemctl'):
            executable = root / name
            executable.parent.mkdir(parents=True, exist_ok=True)
            executable.write_text('#!/bin/sh\nexit 0\n')
            executable.chmod(0o755)
        calendar = subprocess.run(['systemd-analyze', 'calendar', '*-*-* 00:00:00 Europe/Kyiv'],
                                  text=True, capture_output=True, timeout=10)
        self.assertEqual(calendar.returncode, 0, calendar.stderr)
        self.assertIn('Europe/Kyiv', calendar.stdout)
        result = subprocess.run(['systemd-analyze', '--root=' + str(root), '--man=no', 'verify',
                                 'fim.service', 'fim-page.service', 'fim-page.timer',
                                 'fim-config.service', 'fim-config.path', 'fim-failure@fim.service'],
                                text=True, capture_output=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == '__main__':
    unittest.main(verbosity=2)
