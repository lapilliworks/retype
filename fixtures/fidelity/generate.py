#!/usr/bin/env python3
"""Regenerate the fidelity fixtures, `docs/fidelity.md` §4.

Not part of the build. The fixtures are checked in because they are the evidence: the
bytes are the truth, and this script is how they were made. It needs ImageMagick and
python3, and it writes the same bytes every time apart from the ICC profile's
timestamp, which is fixed.

The two files ImageMagick cannot make on its own are written here: a synthetic RGB
profile, so the ICC axis has something to embed rather than a copy of somebody else's
licence, and an EXIF orientation tag, which ImageMagick writes only for images it
already believes are rotated.
"""

import pathlib
import struct
import subprocess
import sys

HERE = pathlib.Path(__file__).resolve().parent
PROFILES = HERE / 'profiles'


def magick(*args):
	subprocess.run(['magick', *args], check=True)


def fixed(value):
	"""A 16.16 fixed-point number, which is what ICC uses for everything."""
	return struct.pack('>i', round(value * 65536))


def xyz(x, y, z):
	return b'XYZ ' + b'\0\0\0\0' + fixed(x) + fixed(y) + fixed(z)


def curve(gamma):
	return b'curv' + b'\0\0\0\0' + struct.pack('>I', 1) + struct.pack('>H', round(gamma * 256))


def desc(text):
	raw = text.encode() + b'\0'
	body = b'desc' + b'\0\0\0\0' + struct.pack('>I', len(raw)) + raw
	body += b'\0\0\0\0' + struct.pack('>I', 0)
	body += struct.pack('>H', 0) + bytes([0]) + b'\0' * 67
	return body


def write_profile(path):
	"""A minimal but valid RGB display profile: what littlecms accepts and PNG embeds."""
	tags = [
		(b'desc', desc('retype fidelity fixture')),
		(b'wtpt', xyz(0.9642, 1.0, 0.8249)),
		(b'rXYZ', xyz(0.4360, 0.2225, 0.0139)),
		(b'gXYZ', xyz(0.3851, 0.7169, 0.0971)),
		(b'bXYZ', xyz(0.1431, 0.0606, 0.7141)),
		(b'rTRC', curve(2.2)),
		(b'gTRC', curve(2.2)),
		(b'bTRC', curve(2.2)),
	]

	offset = 128 + 4 + 12 * len(tags)
	table, data = b'', b''
	for signature, body in tags:
		table += signature + struct.pack('>II', offset, len(body))
		padding = (-len(body)) % 4
		data += body + b'\0' * padding
		offset += len(body) + padding

	size = 128 + 4 + len(table) + len(data)
	header = struct.pack('>I', size) + b'none' + struct.pack('>I', 0x02100000)
	header += b'mntr' + b'RGB ' + b'XYZ '
	header += struct.pack('>6H', 2024, 1, 1, 0, 0, 0) + b'acsp'
	header += b'\0' * 4 + b'\0' * 4 + b'\0' * 4 + b'\0' * 4 + b'\0' * 8
	header += struct.pack('>I', 0)
	header += struct.pack('>III', 0x0000F6D6, 0x00010000, 0x0000D32D)
	header += b'\0' * 4 + b'\0' * 16 + b'\0' * 28
	assert len(header) == 128

	PROFILES.mkdir(parents=True, exist_ok=True)
	path.write_bytes(header + struct.pack('>I', len(tags)) + table + data)


def orient(source, target, value=6):
	"""Insert an EXIF APP1 segment holding one IFD entry: the orientation tag."""
	tiff = b'II' + struct.pack('>H', 42) + struct.pack('<I', 8)
	tiff += struct.pack('<H', 1)
	tiff += struct.pack('<HHI', 0x0112, 3, 1) + struct.pack('<H', value) + b'\0\0'
	tiff += struct.pack('<I', 0)

	payload = b'Exif\0\0' + tiff
	segment = b'\xff\xe1' + struct.pack('>H', len(payload) + 2) + payload

	data = pathlib.Path(source).read_bytes()
	assert data[:2] == b'\xff\xd8', 'not a JPEG'
	pathlib.Path(target).write_bytes(data[:2] + segment + data[2:])


def main():
	profile = PROFILES / 'small-rgb.icc'
	write_profile(profile)

	# A transparent left half and an opaque right half, so flattening is visible.
	magick('-size', '32x32', 'xc:none', '-fill', 'red', '-draw', 'rectangle 16,0 31,31',
		f'PNG32:{HERE / "alpha.png"}')

	# Sixteen bits per channel: a target format that only carries eight has to say so.
	magick('-size', '32x32', 'gradient:red-blue', '-depth', '16', f'PNG48:{HERE / "depth.png"}')

	# The profile is embedded, not tagged in a sidecar: a conversion that drops it
	# changes what the colours mean.
	magick('-size', '32x32', 'gradient:red-blue', '-profile', str(profile),
		f'PNG24:{HERE / "icc.png"}')

	# Three colours in an indexed container: the palette must not gain new ones.
	magick('-size', '32x32', 'xc:#112233', '-fill', '#ff0000', '-draw', 'rectangle 0,0 15,15',
		'-fill', '#00ff00', '-draw', 'rectangle 16,16 31,31', '-colors', '8',
		f'PNG8:{HERE / "palette.png"}')

	# Not square, so a baked rotation is visible in the dimensions.
	base = HERE / 'base.jpg'
	magick('-size', '32x16', 'gradient:red-blue', '-quality', '90', str(base))
	orient(base, HERE / 'orient.jpg')
	base.unlink()

	magick('-delay', '10', '-size', '16x16', 'xc:red', '-size', '16x16', 'xc:green',
		'-size', '16x16', 'xc:blue', '-loop', '0', f'GIF:{HERE / "anim.gif"}')

	magick('-delay', '10', '-size', '16x16', 'xc:red', '-size', '16x16', 'xc:green',
		'-size', '16x16', 'xc:blue', '-loop', '0', f'WEBP:{HERE / "anim.webp"}')

	return 0


if __name__ == '__main__':
	sys.exit(main())
