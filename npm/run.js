#!/usr/bin/env node

const { execFileSync } = require("child_process");
const path = require("path");
const fs = require("fs");

const BINARY = "elisym-mcp";

const PLATFORM_PACKAGES = {
  "darwin-arm64": `@elisym/elisym-mcp-darwin-arm64`,
  "darwin-x64": `@elisym/elisym-mcp-darwin-x64`,
  "linux-x64": `@elisym/elisym-mcp-linux-x64`,
  "linux-arm64": `@elisym/elisym-mcp-linux-arm64`,
};

function getBinaryPath() {
  const key = `${process.platform}-${process.arch}`;
  const pkg = PLATFORM_PACKAGES[key];

  if (!pkg) {
    console.error(
      `Unsupported platform: ${process.platform}-${process.arch}\n` +
        `Supported: ${Object.keys(PLATFORM_PACKAGES).join(", ")}`
    );
    process.exit(1);
  }

  // Try to find the platform-specific package
  try {
    const pkgDir = path.dirname(require.resolve(`${pkg}/package.json`));
    const bin = path.join(pkgDir, BINARY);
    if (fs.existsSync(bin)) {
      return bin;
    }
  } catch {
    // Package not installed — fall through
  }

  // Fallback: check if the binary is in PATH
  const whichCmd = process.platform === "win32" ? "where" : "which";
  try {
    const result = execFileSync(whichCmd, [BINARY], {
      encoding: "utf-8",
      stdio: ["pipe", "pipe", "pipe"],
    }).trim();
    if (result) return result;
  } catch {
    // Not in PATH
  }

  console.error(
    `Could not find ${BINARY} binary.\n\n` +
      `Install options:\n` +
      `  cargo install elisym-mcp\n` +
      `  brew install elisymprotocol/tap/elisym-mcp\n` +
      `  docker run -i --rm elisymprotocol/elisym-mcp\n`
  );
  process.exit(1);
}

const bin = getBinaryPath();

try {
  execFileSync(bin, process.argv.slice(2), {
    stdio: "inherit",
    env: process.env,
  });
} catch (e) {
  if (e.status !== null) {
    process.exit(e.status);
  }
  throw e;
}
