#!/usr/bin/env node

const { spawn } = require("node:child_process");
const fs = require("node:fs");
const path = require("node:path");

const platformPackages = {
  "linux:x64": "wirecli-linux-x64",
  "win32:x64": "wirecli-win32-x64",
};

const packageName = platformPackages[`${process.platform}:${process.arch}`];

if (!packageName) {
  console.error(
    `wirecli has no published prebuilt binary for ${process.platform} ${process.arch}.`
  );
  process.exit(1);
}

let binaryPath;
try {
  const packageJsonPath = require.resolve(`${packageName}/package.json`);
  const packageRoot = path.dirname(packageJsonPath);
  const exe = process.platform === "win32" ? "wirecli-native.exe" : "wirecli-native";
  binaryPath = path.join(packageRoot, "bin", exe);
} catch (error) {
  console.error(`wirecli could not find the platform package ${packageName}.`);
  console.error("Try reinstalling with `npm install -g wirecli@latest`.");
  process.exit(1);
}

if (!fs.existsSync(binaryPath)) {
  console.error(`wirecli platform binary is missing at ${binaryPath}.`);
  process.exit(1);
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
