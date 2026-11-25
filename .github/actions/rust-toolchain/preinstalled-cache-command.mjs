import { spawnSync } from "node:child_process";

const [host, command, ...args] = process.argv.slice(2);
// The existing Linux adapter exposes one active pin. Rust-cache hashes the
// compiler identity queried through that name, so the alias has the same key.
if (process.platform === "linux" && command === "rustup" && args.join(" ") === "toolchain list --quiet") {
  console.log(`stable-${host}`);
} else {
  const child = spawnSync(command, args, {
    env: { ...process.env, RUSTUP_TOOLCHAIN: `stable-${host}`, RUSTUP_AUTO_INSTALL: "0" },
    stdio: "inherit",
    windowsHide: true,
  });
  if (child.error) throw child.error;
  process.exitCode = child.status ?? 1;
}
