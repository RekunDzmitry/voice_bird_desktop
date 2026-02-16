const os = require("os");
const path = require("path");
const fs = require("fs");
const child_process = require("child_process");

const VERSION = require("./package.json").version;

const PLATFORM_MAP = {
  "win32 x64": { pkg: "@voice-bird/cli-win32-x64", bin: "voice-bird-cli.exe" },
  "darwin arm64": { pkg: "@voice-bird/cli-darwin-arm64", bin: "voice-bird-cli" },
  "linux x64": { pkg: "@voice-bird/cli-linux-x64", bin: "voice-bird-cli" },
};

function main() {
  const platformKey = `${process.platform} ${os.arch()}`;
  const entry = PLATFORM_MAP[platformKey];
  if (!entry) return;

  // Check if the platform package was already installed via optionalDependencies
  try {
    require.resolve(path.join(entry.pkg, entry.bin));
    return; // Already installed
  } catch {
    // Not installed, try manual download
  }

  console.log(
    `[voice-bird-cli] Platform package "${entry.pkg}" not found, attempting manual install...`
  );

  try {
    const installDir = path.join(__dirname, ".npm-install-tmp");
    fs.mkdirSync(installDir, { recursive: true });
    fs.writeFileSync(path.join(installDir, "package.json"), "{}");

    child_process.execSync(
      `npm install --loglevel=error --prefer-offline --no-audit --progress=false ${entry.pkg}@${VERSION}`,
      { cwd: installDir, stdio: "pipe" }
    );

    const installedBin = path.join(installDir, "node_modules", entry.pkg, entry.bin);
    const targetDir = path.join(__dirname, "bin");
    fs.mkdirSync(targetDir, { recursive: true });
    const targetPath = path.join(targetDir, entry.bin);
    fs.copyFileSync(installedBin, targetPath);
    fs.chmodSync(targetPath, 0o755);

    fs.rmSync(installDir, { recursive: true, force: true });
    console.log(`[voice-bird-cli] Installed successfully.`);
  } catch (e) {
    console.error(`[voice-bird-cli] Manual install failed: ${e.message}`);
    console.error(`[voice-bird-cli] Download manually from:`);
    console.error(`  https://github.com/RekunDzmitry/voice-bird-releases/releases`);
  }
}

main();
