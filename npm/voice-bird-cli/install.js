const { spawnSync } = require("node:child_process");
const { existsSync, readFileSync, writeFileSync } = require("node:fs");
const { join } = require("node:path");
const { homedir } = require("node:os");

const version = require("./package.json").version;
const binDir = join(homedir(), ".cargo", "bin");
const exe = join(binDir, process.platform === "win32" ? "voice-bird-cli.exe" : "voice-bird-cli");
const marker = join(binDir, ".voice-bird-cli.version");

// We record the version we installed in a marker file next to the binary. The
// native binary has no headless `--version` (it opens an audio device), so the
// marker is how we detect whether the installed build matches this package.
function installedVersion() {
  try {
    return existsSync(exe) ? readFileSync(marker, "utf8").trim() : null;
  } catch {
    return null;
  }
}

// Already on the matching version — nothing to do.
if (installedVersion() === version) {
  process.exit(0);
}

const check = spawnSync("cargo", ["--version"], { stdio: "ignore" });
if (check.status !== 0) {
  console.error("voice-bird-cli npm package requires Rust Cargo. Install Rust from https://rustup.rs/ and rerun npm install.");
  process.exit(1);
}

// Pin to this package's exact version and force, so an existing older binary is
// upgraded rather than left in place (cargo would otherwise no-op on presence).
const install = spawnSync(
  "cargo",
  ["install", "voice-bird-cli", "--version", version, "--locked", "--force"],
  { stdio: "inherit" }
);
if (install.status === 0) {
  try {
    writeFileSync(marker, version + "\n");
  } catch {}
}
process.exit(install.status || 0);
