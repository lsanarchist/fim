#!/usr/bin/env python3
"""Isolated deployment tests: no user creation, live systemctl, MTA or install."""
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

    def installer_fixture(self):
        project = self.root / 'checkout'
        (project / 'target/release').mkdir(parents=True)
        shutil.copytree(PROJECT / 'deploy', project / 'deploy')
        (project / 'site').mkdir()
        shutil.copy2(PROJECT / 'site/style.css', project / 'site/style.css')
        for name in ('config.json', 'namedays.json', 'index.template.html'):
            shutil.copy2(PROJECT / name, project / name)
        binary = project / 'target/release/fim'
        binary.write_text('#!/bin/sh\nexit 0\n')
        binary.chmod(0o755)
        shims = self.root / 'commands'
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
