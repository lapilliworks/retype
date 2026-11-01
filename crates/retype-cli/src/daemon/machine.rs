//! Per-install identity, `docs/dev/architecture/state.md` §2.
//!
//! The identity lives in `bench.toml` beside the calibration, because the two
//! travel together: calibration earned on one machine is not calibration for
//! another. The file belongs to `retype_core::estimate`; this module is the
//! daemon's one call into it.

use std::path::Path;

use crate::error::Result;

pub fn load_or_create(state_dir: &Path) -> Result<String> {
	Ok(retype_core::estimate::machine_identity(state_dir)?)
}
