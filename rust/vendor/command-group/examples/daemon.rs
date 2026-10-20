//! This example shows how to use the `group_spawn` method
//! to spawn a python server daemon in the background.
//!
//! NOTE: This example will not work on Windows, as the
//! `kill_on_drop` flag is not supported via this API.
//!
//! See the `kill_on_drop` example for a Windows-compatible
//! example.

use std::process::Stdio;

use command_group::AsyncCommandGroup;

#[tokio::main]
async fn main() {
	tokio::process::Command::new("python3")
		.args(&["-m", "http.server", "8000"])
		.stderr(Stdio::null())
		.stdout(Stdio::null())
		.group_spawn()
		.expect("failed to spawn server");
}
