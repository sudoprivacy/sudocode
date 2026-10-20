[![Crate release version](https://flat.badgen.net/crates/v/command-group)](https://crates.io/crates/command-group)
[![Crate license: Apache 2.0 or MIT](https://flat.badgen.net/badge/license/Apache%202.0%20or%20MIT)][copyright]
[![CI status](https://github.com/watchexec/command-group/actions/workflows/test.yml/badge.svg)](https://github.com/watchexec/command-group/actions/workflows/test.yml)

# Command Group

_Extension to [`Command`](https://doc.rust-lang.org/std/process/struct.Command.html) to spawn in a process group._

- **[API documentation][docs]**.
- [Dual-licensed][copyright] with Apache 2.0 and MIT.
- Minimum Supported Rust Version: 1.68.0.
  - Only the last five stable versions are supported.
  - MSRV increases within that range at publish time will not incur major version bumps.

[copyright]: ./COPYRIGHT
[docs]: https://docs.rs/command-group

## Quick start

```toml
[dependencies]
command-group = "5.0.1"
```

```rust
use std::process::Command;
use command_group::CommandGroup;

let mut child = Command::new("watch").arg("ls").group_spawn()?;
let status = child.wait()?;
dbg!(status);
```

### Async: Tokio

```toml
[dependencies]
command-group = { version = "5.0.1", features = ["with-tokio"] }
tokio = { version = "1.10.0", features = ["full"] }
```

```rust
use tokio::process::Command;
use command_group::AsyncCommandGroup;

let mut child = Command::new("watch").arg("ls").group_spawn()?;
let status = child.wait().await?;
dbg!(status);
```

Also see the [Examples](./examples)!
