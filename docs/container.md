# Container workflows

`sudocode` ships a checked-in [`Containerfile`](../Containerfile) that
gives Docker and Podman users one canonical container workflow for
building and testing the Rust workspace.

The Rust runtime detects container environments via `/.dockerenv`,
`/run/.containerenv`, matching environment variables, and
`/proc/1/cgroup` hints, and surfaces the detection through
`scode sandbox` and `scode doctor`. The container workflow below uses
this detection as a sanity check.

## What the image is

The image is a reusable Rust build and test shell with the extra
packages this workspace commonly needs (`git`, `pkg-config`,
`libssl-dev`, certificates). The repository is bind-mounted into
`/workspace` at run time; edits stay on the host.

## Build the image

From the repository root:

### Docker

```bash
docker build -t sudocode-dev -f Containerfile .
```

### Podman

```bash
podman build -t sudocode-dev -f Containerfile .
```

## Run `cargo test --workspace` in the container

These commands mount the repo, keep Cargo build artifacts off the
working tree, and run from the Rust workspace at `rust/`.

### Docker

```bash
docker run --rm -it \
  -v "$PWD":/workspace \
  -e CARGO_TARGET_DIR=/tmp/scode-target \
  -w /workspace/rust \
  sudocode-dev \
  cargo test --workspace
```

### Podman

```bash
podman run --rm -it \
  -v "$PWD":/workspace:Z \
  -e CARGO_TARGET_DIR=/tmp/scode-target \
  -w /workspace/rust \
  sudocode-dev \
  cargo test --workspace
```

For a clean rebuild, prefix the command with `cargo clean &&`.

## Open a shell in the container

### Docker

```bash
docker run --rm -it \
  -v "$PWD":/workspace \
  -e CARGO_TARGET_DIR=/tmp/scode-target \
  -w /workspace/rust \
  sudocode-dev
```

### Podman

```bash
podman run --rm -it \
  -v "$PWD":/workspace:Z \
  -e CARGO_TARGET_DIR=/tmp/scode-target \
  -w /workspace/rust \
  sudocode-dev
```

Inside the shell:

```bash
cargo build --workspace
cargo test --workspace
cargo run -p rusty-sudocode-cli -- --help
cargo run -p rusty-sudocode-cli -- sandbox
```

`scode sandbox` is a useful sanity check: inside Docker or Podman it
reports `In container true` and lists the markers the runtime detected.

## Bind-mount this repo and another repo at the same time

To run `scode` against a second checkout while keeping `sudocode`
itself mounted read-write:

### Docker

```bash
docker run --rm -it \
  -v "$PWD":/workspace \
  -v "$HOME/src/other-repo":/repo \
  -e CARGO_TARGET_DIR=/tmp/scode-target \
  -w /workspace/rust \
  sudocode-dev
```

### Podman

```bash
podman run --rm -it \
  -v "$PWD":/workspace:Z \
  -v "$HOME/src/other-repo":/repo:Z \
  -e CARGO_TARGET_DIR=/tmp/scode-target \
  -w /workspace/rust \
  sudocode-dev
```

Then, for example:

```bash
cargo run -p rusty-sudocode-cli -- prompt "summarize /repo"
```

## Notes

- Docker and Podman use the same checked-in `Containerfile`.
- The `:Z` suffix in the Podman examples is for SELinux relabeling on
  Fedora/RHEL-class hosts.
- Running with `CARGO_TARGET_DIR=/tmp/scode-target` keeps
  container-owned `target/` artifacts out of the bind-mounted checkout.
- For host-side workflows, see [`usage.md`](./usage.md) and
  [`../rust/README.md`](../rust/README.md).
