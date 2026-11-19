import { spawnSync } from "node:child_process";

const [pin, command, ...args] = process.argv.slice(2);
// rust-cache separately fingerprints the real `rustc -vV`. Only narrow its
// inventory query, and keep the original list if the exact pin is not active.
if (command === "rustup" && args.length === 3 && args.join(" ") === "toolchain list --quiet") {
  const active = spawnSync("rustup", ["show", "active-toolchain"], { encoding: "utf8", windowsHide: true });
  const name = active.stdout?.trim().split(/\s+/, 1)[0];
  if (active.status === 0 && /^\d+\.\d+\.\d+$/.test(pin) && name?.startsWith(pin + "-")) {
    console.log(name);
    process.exit(0);
  }
}

const child = spawnSync(command, args, { stdio: "inherit", windowsHide: true });
if (child.error) throw child.error;
process.exit(child.status ?? 1);
