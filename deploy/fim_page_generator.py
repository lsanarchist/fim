#!/usr/bin/env python3
"""Generate the publisher timer from trusted config, at boot and daemon-reload."""
import argparse
import json
import os
import re
import sys
import tempfile
from pathlib import Path
from zoneinfo import ZoneInfo, ZoneInfoNotFoundError


def configured_timezone(config):
    with config.open('rb') as handle:
        content = handle.read(1024 * 1024 + 1)
    if len(content) > 1024 * 1024:
        raise ValueError('Configuration exceeds 1 MiB')
    zone = json.loads(content)['timezone']
    # Keep systemd directives and specifiers out, even if a future timezone
    # database accepts a more permissive key. ZoneInfo also checks existence.
    if not isinstance(zone, str) or not re.fullmatch(r'[A-Za-z0-9._+-]+(?:/[A-Za-z0-9._+-]+)*', zone):
        raise ValueError('timezone must be an IANA timezone name')
    if any(part in ('.', '..') for part in zone.split('/')):
        raise ValueError('Invalid timezone path')
    try:
        ZoneInfo(zone)
    except (ZoneInfoNotFoundError, ValueError) as error:
        raise ValueError(f'Unknown IANA timezone: {zone}') from error
    return zone


def write_override(output, zone=None):
    directory = output / 'fim-page.timer.d'
    directory.mkdir(parents=True, exist_ok=True)
    text = '[Timer]\nOnCalendar=\n'
    if zone is not None:
        text += f'OnCalendar=*-*-* 00:00:00 {zone}\n'
    descriptor, temporary = tempfile.mkstemp(prefix='.timezone-', dir=directory)
    try:
        with os.fdopen(descriptor, 'w') as handle:
            handle.write(text)
            handle.flush()
            os.fchmod(handle.fileno(), 0o644)
        os.replace(temporary, directory / '20-timezone.conf')
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, default=Path('/opt/fim/config.json'))
    parser.add_argument('--check-config', type=Path, help='Validate only; do not create any files')
    parser.add_argument('output', nargs='?', type=Path)
    parser.add_argument('early', nargs='?')
    parser.add_argument('late', nargs='?')
    args = parser.parse_args()
    try:
        if args.check_config is not None:
            print(configured_timezone(args.check_config))
            return 0
        if args.output is None:
            parser.error('systemd generator output directory is required')
        try:
            zone = configured_timezone(args.config)
        except (OSError, ValueError, KeyError, TypeError):
            # Invalid config must never retain a previous timezone or fall back
            # to host time. A timer without a schedule fails closed.
            write_override(args.output)
            raise
        write_override(args.output, zone)
        return 0
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f'FIM timer configuration: {error}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    sys.exit(main())
