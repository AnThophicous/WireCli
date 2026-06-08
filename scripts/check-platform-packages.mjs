#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const repoRoot = path.resolve(path.dirname(new URL(import.meta.url).pathname), "..");
const required = [
  ["linux-x64", "wirecli-native"],
  ["win32-x64", "wirecli-native.exe"],
];

let failed = false;

for (const [platformName, binaryName] of required) {
  const binaryPath = path.join(repoRoot, "packages", platformName, "bin", binaryName);
  const licensePath = path.join(repoRoot, "packages", platformName, "LICENSE");
  if (!fs.existsSync(binaryPath)) {
    console.error(`missing binary for ${platformName}: ${binaryPath}`);
    failed = true;
    continue;
  }
  if (!fs.existsSync(licensePath)) {
    console.error(`missing LICENSE for ${platformName}: ${licensePath}`);
    failed = true;
    continue;
  }
  const size = fs.statSync(binaryPath).size;
  console.log(`${platformName}: ok (${size} bytes)`);
}

if (failed) {
  process.exit(1);
}
