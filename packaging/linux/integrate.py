#!/usr/bin/env python3
"""Install or remove Retype's per-user Linux desktop integration."""
import argparse
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import stat
import subprocess
import tempfile

DESKTOP = 'retype-uri.desktop'
SCHEME = 'x-scheme-handler/retype'
SECTIONS = {'[Default Applications]', '[Added Associations]', '[Removed Associations]'}
OWNER = '# Retype installation: '


def run(*command, check=True, env=None):
	return subprocess.run(command, check=check, text=True, capture_output=True, timeout=300, env=env)


def write(path, contents, mode=None):
	path.parent.mkdir(parents=True, exist_ok=True)
	if mode is None:
		mode = stat.S_IMODE(path.stat().st_mode) if path.is_file() else 0o644
	with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as staged:
		staged.write(contents)
		staged.flush()
		os.fchmod(staged.fileno(), mode)
	try:
		os.replace(staged.name, path)
	finally:
		Path(staged.name).unlink(missing_ok=True)


def save_json(path, value):
	write(path, (json.dumps(value, indent=2) + '\n').encode(), 0o600)


def snapshot(path):
	if path.is_symlink():
		return {'link': os.readlink(path)}
	if not path.exists():
		return None
	if not path.is_file():
		raise RuntimeError(f'Refusing to replace a non-file: {path}')
	return {'contents': path.read_bytes().hex(), 'mode': stat.S_IMODE(path.stat().st_mode)}


def restore(path, saved):
	if saved is None:
		path.unlink(missing_ok=True)
	elif 'link' in saved:
		path.unlink(missing_ok=True)
		path.parent.mkdir(parents=True, exist_ok=True)
		path.symlink_to(saved['link'])
	else:
		write(path, bytes.fromhex(saved['contents']))
		path.chmod(saved['mode'])


def scheme_entries(text):
	section = ''
	entries = {}
	for line in text.splitlines(keepends=True):
		if line.strip().startswith('['):
			section = line.strip()
		elif section in SECTIONS and line.partition('=')[0].strip() == SCHEME:
			entries.setdefault(section, []).append(line)
	return entries


def replace_scheme(text, entries):
	if text and not text.endswith('\n'):
		text += '\n'
	section, result, pending = '', [], dict(entries)
	for line in text.splitlines(keepends=True):
		if line.strip().startswith('['):
			result.extend(pending.pop(section, []))
			section = line.strip()
		if section in SECTIONS and line.partition('=')[0].strip() == SCHEME:
			continue
		result.append(line)
	result.extend(pending.pop(section, []))
	for section, lines in pending.items():
		if result and not result[-1].endswith('\n'):
			result.append('\n')
		result.extend(['\n', section + '\n', *lines])
	return ''.join(result)


def text(path):
	return path.read_text() if path.exists() else ''


def locations(prefix):
	config = Path(os.environ.get('XDG_CONFIG_HOME') or Path.home() / '.config')
	data = Path(os.environ.get('XDG_DATA_HOME') or Path.home() / '.local/share')
	for path in (prefix, config, data):
		if not path.is_absolute() or any(character in str(path) for character in '\n\r\t'):
			raise RuntimeError(f'Expected an absolute path without control characters: {path}')
	return config, data


def preference_paths(config):
	paths = [config / 'mimeapps.list']
	for desktop in os.environ.get('XDG_CURRENT_DESKTOP', '').lower().split(':'):
		if re.fullmatch('[a-z0-9_-]+', desktop):
			path = config / f'{desktop}-mimeapps.list'
			if path.exists():
				paths.append(path)
	return list(dict.fromkeys(path.resolve() for path in paths))


def render(template, prefix, desktop=False):
	value = str(prefix).replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%')
	if desktop:
		value = value.replace('$', '\\$').replace('`', '\\`').replace('\\', '\\\\')
	else:
		value = value.replace('$', '$$')
	header = OWNER + json.dumps(str(prefix)) + '\n'
	return (header + template.read_text().replace('@PREFIX@', value)).encode()


def installation_owner(path):
	if path.is_file():
		for line in path.read_text().splitlines():
			if line.startswith(OWNER):
				return Path(json.loads(line[len(OWNER):]))
	return None


def manager_available(config):
	if not shutil.which('systemctl'):
		return False
	response = run('systemctl', '--user', 'show-environment', check=False)
	if response.returncode != 0:
		return False
	environment = dict(item.split('=', 1) for item in shlex.split(response.stdout) if '=' in item)
	home = environment.get('HOME')
	manager_config = environment.get('XDG_CONFIG_HOME') or (str(Path(home) / '.config') if home else '')
	return bool(manager_config) and Path(manager_config).resolve() == config.resolve()


def refresh(data, manager):
	if shutil.which('update-desktop-database'):
		run('update-desktop-database', str(data / 'applications'))
	if manager:
		run('systemctl', '--user', 'daemon-reload')


def remember(journal, path):
	key = str(path)
	if key not in journal['files']:
		journal['files'][key] = snapshot(path)


def installation(prefix, backup, operation):
	config, data = locations(prefix)
	share = prefix / 'share/retype'
	metadata = share / 'installation.json'
	old_metadata = backup / 'share/installation.json' if operation == 'install' else metadata
	previous = json.loads(old_metadata.read_text()) if old_metadata.exists() else None
	if previous and (previous['config'] != str(config) or previous['data'] != str(data)):
		raise RuntimeError('Use the same XDG_CONFIG_HOME and XDG_DATA_HOME as the original installation')
	paths = preference_paths(config)
	if previous:
		paths = list(dict.fromkeys([*map(Path, previous['preferences']), *paths]))
	unit = config / 'systemd/user/retype.service'
	desktop = data / 'applications' / DESKTOP
	for path in (unit, desktop):
		owner = installation_owner(path)
		if owner is not None and owner.resolve() != prefix.resolve():
			raise RuntimeError(f'{path} belongs to Retype under {owner}; update or remove that installation first')
	manager = manager_available(config)
	journal = {'files': {}, 'data': str(data), 'manager': manager,
		'active': manager and run('systemctl', '--user', 'is-active', '--quiet', 'retype.service', check=False).returncode == 0,
		'enabled': False}
	for path in (unit, desktop, *paths):
		remember(journal, path)
	journal_path = backup / 'integration.json'
	save_json(journal_path, journal)
	if operation == 'remove':
		if previous is None:
			raise RuntimeError('This installation has no Linux integration record')
		remove(previous, journal, journal_path, unit, desktop, data, manager)
		return
	state = previous or {'config': str(config), 'data': str(data), 'files': {}, 'preferences': {}}
	for path, template, is_desktop in ((unit, 'retype.service', False), (desktop, DESKTOP, True)):
		key = str(path)
		old = state['files'].get(key)
		if old and snapshot(path) != old['installed']:
			print(f'Preserved modified file: {path}')
			continue
		before = old['before'] if old else snapshot(path)
		write(path, render(share / 'packaging/linux' / template, prefix, is_desktop))
		state['files'][key] = {'before': before, 'installed': snapshot(path)}
	register = previous is None or run('xdg-mime', 'query', 'default', SCHEME).stdout.strip() == DESKTOP
	if register:
		preferences(state, paths)
	refresh(data, manager)
	if journal['active']:
		run('systemctl', '--user', 'try-restart', 'retype.service')
	save_json(metadata, state)
	print(f'Linux desktop integration installed; user-service enablement unchanged: {unit}')


def preferences(state, paths):
	before = {str(path): scheme_entries(text(path)) for path in paths}
	# Stage xdg-mime's edit so a symlinked preference file and its unrelated entries survive.
	with tempfile.TemporaryDirectory(prefix='retype-mime-') as temporary:
		staged = Path(temporary) / 'mimeapps.list'
		staged.write_text(text(paths[0]))
		run('xdg-mime', 'default', DESKTOP, SCHEME, env={**os.environ, 'XDG_CONFIG_HOME': temporary})
		defaults = scheme_entries(staged.read_text())
	for index, path in enumerate(paths):
		key = str(path)
		entries = scheme_entries(text(path))
		if index == 0:
			entries = defaults
		elif '[Default Applications]' in entries:
			entries['[Default Applications]'] = [f'{SCHEME}={DESKTOP};\n']
		write(path, replace_scheme(text(path), entries).encode())
		old = state['preferences'].get(key)
		state['preferences'][key] = {'before': old['before'] if old else before[key],
			'installed': scheme_entries(text(path))}
	if run('xdg-mime', 'query', 'default', SCHEME).stdout.strip() != DESKTOP:
		raise RuntimeError('The desktop did not accept the Retype URI association')


def stop_service(unit, journal, journal_path, manager):
	if manager:
		journal['enabled'] = run('systemctl', '--user', 'is-enabled', 'retype.service', check=False).stdout.strip()
		save_json(journal_path, journal)
		run('systemctl', '--user', 'stop', 'retype.service')
		run('systemctl', '--user', 'disable', 'retype.service')
	else:
		# A live manager for another XDG configuration must not receive this installation's commands.
		for pattern in ('*.wants/retype.service', '*.requires/retype.service'):
			for link in unit.parent.glob(pattern):
				if link.is_symlink() and link.resolve() == unit.resolve():
					remember(journal, link)
					save_json(journal_path, journal)
					link.unlink()


def remove(state, journal, journal_path, unit, desktop, data, manager):
	unit_record = state['files'].get(str(unit))
	if unit_record and unit.exists():
		stop_service(unit, journal, journal_path, manager)
	for key, entry in state['preferences'].items():
		path = Path(key)
		current = text(path)
		if scheme_entries(current) == entry['installed']:
			write(path, replace_scheme(current, entry['before']).encode())
		else:
			entries = scheme_entries(current)
			if entries.get('[Default Applications]') == entry['installed'].get('[Default Applications]'):
				entries['[Default Applications]'] = entry['before'].get('[Default Applications]', [])
			if state['files'][str(desktop)]['before'] is None:
				for section, lines in entries.items():
					remaining = []
					for line in lines:
						key, _, value = line.partition('=')
						apps = [app for app in value.strip().split(';') if app and app != DESKTOP]
						if apps:
							remaining.append(key + '=' + ';'.join(apps) + ';\n')
					entries[section] = remaining
			write(path, replace_scheme(current, entries).encode())
			print(f'Preserved changed URI preferences: {path}')
	for key, entry in state['files'].items():
		path = Path(key)
		if snapshot(path) == entry['installed']:
			restore(path, entry['before'])
		else:
			print(f'Preserved modified file: {path}')
	refresh(data, manager)


def rollback(backup):
	journal_path = backup / 'integration.json'
	if not journal_path.exists():
		return
	journal = json.loads(journal_path.read_text())
	for key, saved in journal['files'].items():
		restore(Path(key), saved)
	refresh(Path(journal['data']), journal['manager'])
	if journal['enabled'] in ('enabled', 'enabled-runtime'):
		options = ['--runtime'] if journal['enabled'] == 'enabled-runtime' else []
		run('systemctl', '--user', 'enable', *options, 'retype.service')
	if journal['active']:
		run('systemctl', '--user', 'restart', 'retype.service')


def main():
	parser = argparse.ArgumentParser(description=__doc__)
	parser.add_argument('operation', choices=['check', 'install', 'remove', 'rollback'])
	parser.add_argument('--prefix', required=True, type=Path)
	parser.add_argument('--backup', type=Path)
	args = parser.parse_args()
	if args.operation == 'check':
		locations(args.prefix)
		if not shutil.which('xdg-mime'):
			raise RuntimeError('Linux desktop integration needs xdg-mime from the xdg-utils package')
		return
	if args.backup is None:
		parser.error('--backup is required')
	if args.operation == 'rollback':
		rollback(args.backup)
	else:
		installation(args.prefix, args.backup, args.operation)


if __name__ == '__main__':
	try:
		main()
	except (OSError, RuntimeError, subprocess.SubprocessError, ValueError, KeyError) as error:
		raise SystemExit(f'install: Linux integration failed: {error}') from error
