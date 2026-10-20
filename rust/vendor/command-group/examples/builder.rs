//! This example shows the basic usage of the sync version
//! of the `group` method, and collects the exit code. The
//! group builder gives more control over the process group.

use command_group::CommandGroup;

fn main() {
	let mut handle = std::process::Command::new("ls").group().spawn().unwrap();
	println!("{:?}", handle);
	let exit_code = handle.wait();
	println!("{:?}", exit_code);
}
