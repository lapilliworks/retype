//! Whole-second archive modification times; ZIP wall-clock fields are interpreted as UTC.

use crate::error::{Error, Result};

pub(super) fn from_zip(entry: &zip::read::ZipFile<'_, std::fs::File>) -> Result<u64> {
	use zip::extra_fields::ExtraField;
	for field in entry.extra_data_fields() {
		match field {
			ExtraField::ExtendedTimestamp(time) => {
				if let Some(seconds) = time.mod_time() {
					return Ok(u64::from(seconds));
				}
			}
			ExtraField::Ntfs(time) => {
				let ticks = time.mtime().checked_sub(116_444_736_000_000_000);
				if let Some(ticks) = ticks.filter(|ticks| ticks % 10_000_000 == 0) {
					return Ok(ticks / 10_000_000);
				}
				return Err(Error::Io(std::io::Error::other(
					"NTFS modification time cannot be preserved",
				)));
			}
		}
	}
	let time = entry
		.last_modified()
		.ok_or_else(|| Error::Io(std::io::Error::other("archive entry has no valid modification time")))?;
	Ok(to_seconds(time))
}

pub(super) fn from_tar<R: std::io::Read>(entry: &mut tar::Entry<'_, R>) -> Result<u64> {
	let mut seconds = entry.header().mtime()?;
	if let Some(extensions) = entry.pax_extensions()? {
		for extension in extensions {
			let extension = extension?;
			if extension.key_bytes() == b"mtime" {
				seconds = std::str::from_utf8(extension.value_bytes())
					.ok()
					.and_then(|value| value.parse().ok())
					.ok_or_else(|| super::refused("the entry", "PAX modification time is not whole UTC seconds"))?;
			}
		}
	}
	Ok(seconds)
}

fn month_days(year: u16) -> [u64; 12] {
	let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
	[31, 28 + u64::from(leap), 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
}

fn to_seconds(time: zip::DateTime) -> u64 {
	let years: u64 = (1970..time.year())
		.map(|year| month_days(year).iter().sum::<u64>())
		.sum();
	let months: u64 = month_days(time.year())[..usize::from(time.month() - 1)].iter().sum();
	let days = years + months + u64::from(time.day() - 1);
	days * 86_400 + u64::from(time.hour()) * 3600 + u64::from(time.minute()) * 60 + u64::from(time.second())
}

pub(super) fn options(mode: u32, seconds: u64) -> Result<zip::write::FullFileOptions<'static>> {
	if mode & 0o7000 != 0 {
		return Err(super::refused(
			"the entry",
			"ZIP writing cannot preserve special permission bits",
		));
	}
	let seconds = u32::try_from(seconds)
		.map_err(|_| super::refused("the entry", "ZIP modification time exceeds its UTC timestamp range"))?;
	let mut options = zip::write::FullFileOptions::default()
		.compression_method(zip::CompressionMethod::Deflated)
		.large_file(true)
		.unix_permissions(mode)
		.last_modified_time(to_zip(seconds));
	/* The UTC extra field preserves odd seconds and dates before the DOS epoch. */
	let mut timestamp = vec![1];
	timestamp.extend_from_slice(&seconds.to_le_bytes());
	options
		.add_extra_data(0x5455, timestamp.into_boxed_slice(), false)
		.map_err(|error| super::unwritable(&error))?;
	Ok(options)
}

fn to_zip(seconds: u32) -> zip::DateTime {
	if u64::from(seconds) < to_seconds(zip::DateTime::default()) {
		return zip::DateTime::default();
	}
	let mut days = u64::from(seconds) / 86_400;
	let mut year = 1970;
	while days >= month_days(year).iter().sum() {
		days -= month_days(year).iter().sum::<u64>();
		year += 1;
	}
	let mut month = 1;
	for length in month_days(year) {
		if days < length {
			break;
		}
		days -= length;
		month += 1;
	}
	let within_day = seconds % 86_400;
	zip::DateTime::from_date_and_time(
		year,
		month,
		days as u8 + 1,
		(within_day / 3600) as u8,
		(within_day / 60 % 60) as u8,
		(within_day % 60) as u8,
	)
	.expect("u32 UTC seconds fit the DOS date range after 1980")
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn calendar_conversion_handles_leap_centuries_and_range_edges() {
		for (year, month, day) in [(1980, 1, 1), (2000, 2, 29), (2100, 3, 1), (2106, 2, 7)] {
			let time = zip::DateTime::from_date_and_time(year, month, day, 1, 2, 4).unwrap();
			assert_eq!(to_zip(to_seconds(time) as u32), time);
		}
		assert_eq!(to_seconds(zip::DateTime::default()), 315_532_800);
		assert_eq!(to_zip(0), zip::DateTime::default());
		assert_eq!(to_seconds(to_zip(u32::MAX)), u64::from(u32::MAX) - 1);
		assert!(options(0o1777, 0).is_err());
		assert!(options(0o644, u64::from(u32::MAX) + 1).is_err());
	}
}
