#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";

const [, , platformName, sourcePath] = process.argv;

if (!platformName || !sourcePath) {
  console.error(
    "usage: node scripts/stage-platform-binary.mjs <linux-x64|win32-x64> <source-binary>"
  );
  process.exit(1);
}

const repoRoot = path.resolve(path.dirname(new URL(import.meta.url).pathname), "..");
const packageDir = path.join(repoRoot, "packages", platformName);
const outputDir = path.join(packageDir, "bin");
const outputName = platformName === "win32-x64" ? "wirecli-native.exe" : "wirecli-native";
const outputPath = path.join(outputDir, outputName);

if (!fs.existsSync(packageDir)) {
  console.error(`unknown platform package: ${platformName}`);
  process.exit(1);
}

if (!fs.existsSync(sourcePath)) {
  console.error(`source binary not found: ${sourcePath}`);
  process.exit(1);
}

fs.mkdirSync(outputDir, { recursive: true });
fs.copyFileSync(sourcePath, outputPath);

if (platformName !== "win32-x64") {
  fs.chmodSync(outputPath, 0o755);
}

console.log(`staged ${platformName} binary -> ${outputPath}`);
