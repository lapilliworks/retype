//! `retype init`: the example configuration, `docs/cli.md` §1.
//!
//! The file is small on purpose. Every key in it is one this build reads, so a
//! user who starts here starts from something that works and can be deleted key
//! by key; a file full of commented examples of features that do not exist yet
//! would be documentation pretending to be configuration.

use std::path::Path;
use std::process::ExitCode;

use crate::error::{Error, Result};

/// The starting configuration. It is validated by the tests, so a change that
/// would not load fails the build rather than the user's first run.
pub const EXAMPLE: &str = "\
# retype configuration, merged over the built-in table: anything left out keeps
# its shipped value, and `enabled = false` removes an entry.
version = 'retype-config-1'

# Which external tool is tried first for each conversion.
[backends]
prefer = ['magick', 'ffmpeg', 'pandoc', '7zz']

# How long undo records are kept, which is also how long a partial job can be
# resumed.
[undo]
retention = '1w'

# Record the intent on the file as an extended attribute, where the platform has
# them, so `retype why` still answers after the journal has been pruned.
[provenance]
xattr = true
";

pub fn run(output: Option<&Path>, quiet: bool) -> Result<ExitCode> {
	match output {
		None => print!("{EXAMPLE}"),
		Some(path) => {
			// Overwriting a configuration that exists is the one thing this command
			// must never do quietly: the file may be the only record of how a
			// machine was set up.
			if path.exists() {
				return Err(Error::Core(retype_core::error::Error::Exists(path.to_path_buf())));
			}

			if let Some(parent) = path.parent() {
				std::fs::create_dir_all(parent)?;
			}

			std::fs::write(path, EXAMPLE)?;

			if !quiet {
				eprintln!("retype: wrote {}", path.display());
			}
		}
	}

	Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
	#[test]
	fn the_example_is_configuration_this_build_loads() {
		retype_core::registry::Registry::with_user(super::EXAMPLE, "the example").expect("the example must load");
	}
}
