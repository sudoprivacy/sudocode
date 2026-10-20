//! This example shows how to use the `group` builder
//! to spawn a python server daemon in the background,
//! while setting creation flags on windows to hide the
//! console window.

use std::process::Stdio;

use command_group::AsyncCommandGroup;
use winapi::um::winbase::CREATE_NO_WINDOW;

#[tokio::main]
async fn main() {
	let group = tokio::process::Command::new("python3")
		.args(&["-m", "http.server", "8000"])
		.stderr(Stdio::null())
		.stdout(Stdio::null())
		.group()
		.creation_flags(CREATE_NO_WINDOW)
		.spawn();
}
