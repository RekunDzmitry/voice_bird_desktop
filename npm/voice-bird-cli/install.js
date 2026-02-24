const os = require("os");
const path = require("path");
const fs = require("fs");
const child_process = require("child_process");

const VERSION = require("./package.json").version;

const PLATFORM_MAP = {
  "win32 x64": { pkg: "@voice-bird/cli-win32-x64", bin: "voice-bird-cli.exe" },
  "darwin arm64": {
    pkg: "@voice-bird/cli-darwin-arm64",
    bin: "VoiceBirdCLI.app/Contents/MacOS/voice-bird-cli",
    app: "VoiceBirdCLI.app",
  },
  "linux x64": { pkg: "@voice-bird/cli-linux-x64", bin: "voice-bird-cli" },
};

function copyDirSync(src, dest) {
  fs.mkdirSync(dest, { recursive: true });
  for (const entry of fs.readdirSync(src, { withFileTypes: true })) {
    const srcPath = path.join(src, entry.name);
    const destPath = path.join(dest, entry.name);
    if (entry.isDirectory()) {
      copyDirSync(srcPath, destPath);
    } else {
      fs.copyFileSync(srcPath, destPath);
      // Preserve executable permissions
      const stat = fs.statSync(srcPath);
      fs.chmodSync(destPath, stat.mode);
    }
  }
}

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

    const targetDir = path.join(__dirname, "bin");
    fs.mkdirSync(targetDir, { recursive: true });

    if (entry.app) {
      // Copy the entire .app bundle directory
      const installedApp = path.join(installDir, "node_modules", entry.pkg, entry.app);
      const targetApp = path.join(targetDir, entry.app);
      copyDirSync(installedApp, targetApp);
    } else {
      // Copy single binary
      const installedBin = path.join(installDir, "node_modules", entry.pkg, entry.bin);
      const targetPath = path.join(targetDir, entry.bin);
      fs.copyFileSync(installedBin, targetPath);
      fs.chmodSync(targetPath, 0o755);
    }

    fs.rmSync(installDir, { recursive: true, force: true });
    console.log(`[voice-bird-cli] Installed successfully.`);
  } catch (e) {
    console.error(`[voice-bird-cli] Manual install failed: ${e.message}`);
    console.error(`[voice-bird-cli] Download manually from:`);
    console.error(`  https://github.com/RekunDzmitry/voice-bird-releases/releases`);
  }
}

main();
