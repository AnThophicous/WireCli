#!/usr/bin/env node

const fs = require("node:fs");
const https = require("node:https");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const REPO = "AnThophicous/WireCli";
const ROOT = path.resolve(__dirname, "..");
const VENDOR_DIR = path.join(ROOT, "vendor");
const PACKAGE_VERSION = require("../package.json").version;

const TARGETS = {
  "linux:x64": {
    asset: "wirecli-linux-x64",
    executable: "wirecli",
  },
  "win32:x64": {
    asset: "wirecli-win32-x64.exe",
    executable: "wirecli.exe",
  },
};

function currentTarget() {
  const target = TARGETS[`${process.platform}:${process.arch}`];
  if (!target) {
    throw new Error(`wirecli does not ship a prebuilt binary for ${process.platform} ${process.arch}`);
  }
  return target;
}

function binaryPath() {
  return path.join(VENDOR_DIR, currentTarget().executable);
}

function downloadUrl() {
  if (process.env.WIRECLI_DOWNLOAD_URL) {
    return process.env.WIRECLI_DOWNLOAD_URL;
  }
  const target = currentTarget();
  return `https://github.com/${REPO}/releases/download/v${PACKAGE_VERSION}/${target.asset}`;
}

function installSync() {
  return spawnSync(process.execPath, [__filename], {
    cwd: ROOT,
    stdio: "inherit",
    env: process.env,
  });
}

function download(url, destination, redirects = 0) {
  if (redirects > 5) {
    return Promise.reject(new Error("too many redirects while downloading wirecli"));
  }

  return new Promise((resolve, reject) => {
    const request = https.get(
      url,
      {
        headers: {
          "User-Agent": "wirecli-npm-installer",
        },
      },
      (response) => {
        const status = response.statusCode ?? 0;
        const location = response.headers.location;

        if (status >= 300 && status < 400 && location) {
          response.resume();
          const nextUrl = new URL(location, url).toString();
          download(nextUrl, destination, redirects + 1).then(resolve, reject);
          return;
        }

        if (status !== 200) {
          response.resume();
          reject(new Error(`download failed with HTTP ${status}: ${url}`));
          return;
        }

        const tmp = `${destination}.tmp-${process.pid}`;
        const file = fs.createWriteStream(tmp, { mode: 0o755 });

        response.pipe(file);
        file.on("finish", () => {
          file.close((error) => {
            if (error) {
              reject(error);
              return;
            }
            fs.renameSync(tmp, destination);
            if (process.platform !== "win32") {
              fs.chmodSync(destination, 0o755);
            }
            resolve();
          });
        });
        file.on("error", (error) => {
          fs.rmSync(tmp, { force: true });
          reject(error);
        });
      }
    );

    request.on("error", reject);
  });
}

async function install() {
  const target = currentTarget();
  const destination = binaryPath();
  fs.mkdirSync(VENDOR_DIR, { recursive: true });

  if (fs.existsSync(destination) && fs.statSync(destination).size > 0) {
    return destination;
  }

  const url = downloadUrl();
  console.log(`[wirecli] downloading ${target.asset} from GitHub Releases`);
  await download(url, destination);

  if (!fs.existsSync(destination) || fs.statSync(destination).size === 0) {
    throw new Error(`wirecli binary was not installed at ${destination}`);
  }

  console.log(`[wirecli] installed ${destination}`);
  return destination;
}

if (require.main === module) {
  install().catch((error) => {
    console.error(`[wirecli] ${error.message}`);
    console.error("[wirecli] Reinstall with `npm install -g wirecli@latest` after checking network access.");
    process.exit(1);
  });
}

module.exports = {
  binaryPath,
  install,
  installSync,
};
