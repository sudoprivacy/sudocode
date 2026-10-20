//! This example shows the basic usage of the sync version
//! of the `group_spawn` method, and collects the exit code.

use command_group::CommandGroup;

fn main() {
	let mut handle = std::process::Command::new("ls").group_spawn().unwrap();
	println!("{:?}", handle);
	let exit_code = handle.wait();
	println!("{:?}", exit_code);
}
