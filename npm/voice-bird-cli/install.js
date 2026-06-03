const { spawnSync } = require("node:child_process");
const { existsSync } = require("node:fs");
const { join } = require("node:path");
const { homedir } = require("node:os");

const exe = join(homedir(), ".cargo", "bin", process.platform === "win32" ? "voice-bird-cli.exe" : "voice-bird-cli");

if (existsSync(exe)) {
  process.exit(0);
}

const check = spawnSync("cargo", ["--version"], { stdio: "ignore" });
if (check.status !== 0) {
  console.error("voice-bird-cli npm package requires Rust Cargo. Install Rust from https://rustup.rs/ and rerun npm install.");
  process.exit(1);
}

const install = spawnSync("cargo", ["install", "voice-bird-cli", "--locked"], { stdio: "inherit" });
process.exit(install.status || 0);
