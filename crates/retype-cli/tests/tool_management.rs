//! Tool management belongs to the GUI, `docs/tools.md` §2.

use std::process::Command;

fn retype() -> Command {
	Command::new(env!("CARGO_BIN_EXE_retype"))
}

#[test]
fn the_cli_has_no_tool_management_command() {
	let ran = retype().args(["tools", "list"]).output().unwrap();

	assert_eq!(ran.status.code(), Some(2), "the subcommand does not exist");
}
