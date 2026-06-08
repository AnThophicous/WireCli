#!/usr/bin/env node

const { spawn } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const installer = require("../scripts/install-release-binary");
const binaryPath = installer.binaryPath();

if (!fs.existsSync(binaryPath)) {
  const result = installer.installSync();
  if (result.status !== 0) {
    process.exit(result.status ?? 1);
  }
}

const child = spawn(binaryPath, process.argv.slice(2), {
  stdio: "inherit",
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 0);
});

child.on("error", (error) => {
  console.error(`failed to start wirecli: ${error.message}`);
  process.exit(1);
});
