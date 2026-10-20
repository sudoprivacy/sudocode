//! This example shows how to use the `group` builder
//! to spawn a python server daemon in the background,
//! while supporting windows by explicitly setting the
//! `kill_on_drop` flag.

use std::process::Stdio;

use command_group::AsyncCommandGroup;

#[tokio::main]
async fn main() {
	tokio::process::Command::new("python3")
		.args(&["-m", "http.server", "8000"])
		.stderr(Stdio::null())
		.stdout(Stdio::null())
		.group()
		.kill_on_drop(true)
		.spawn()
		.expect("failed to spawn server");
}
