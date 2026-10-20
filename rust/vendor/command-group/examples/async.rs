//! This example shows the basic usage of the async version
//! of the `group_spawn` method, and collects the exit code.

use command_group::AsyncCommandGroup;

#[tokio::main]
async fn main() {
	let mut handle = tokio::process::Command::new("ls").group_spawn().unwrap();
	println!("{:?}", handle);
	let exit_code = handle.wait().await;
	println!("{:?}", exit_code);
}
