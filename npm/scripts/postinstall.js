#!/usr/bin/env node
"use strict";

// Brick npm installer.
//
// The npm package is a thin shim: it ships no binary itself. On install this
// script detects the host platform, downloads the matching prebuilt `brick`
// binary from the GitHub Release whose tag is `v<package version>`, and drops it
// at bin/brick-native so the bin/brick.js shim can exec it.
//
// Kept dependency-free (Node core only) so a fresh `npm i` needs nothing else.

const fs = require("fs");
const path = require("path");
const https = require("https");
const { createHash } = require("crypto");

const pkg = require("../package.json");
const REPO = "yorgai/Brick-Vault";
const VERSION = pkg.version;

// Allow installs to skip the download (e.g. CI, air-gapped, or when a system
// `brick` is already on PATH). The shim falls back to PATH in that case.
if (process.env.BRICK_SKIP_DOWNLOAD === "1") {
  console.log("brick: BRICK_SKIP_DOWNLOAD=1 set, skipping binary download.");
  process.exit(0);
}

/** Maps Node's process.platform/arch to a Release asset target triple. */
function resolveTarget() {
  const platform = process.platform;
  const arch = process.arch;
  const map = {
    "darwin:arm64": "aarch64-apple-darwin",
    "darwin:x64": "x86_64-apple-darwin",
    "linux:x64": "x86_64-unknown-linux-gnu",
    "linux:arm64": "aarch64-unknown-linux-gnu",
    "win32:x64": "x86_64-pc-windows-msvc",
  };
  const key = `${platform}:${arch}`;
  const triple = map[key];
  if (!triple) {
    return null;
  }
  const ext = platform === "win32" ? ".exe" : "";
  return { triple, ext, isWindows: platform === "win32" };
}

/** GETs a URL into a Buffer, following redirects (GitHub Releases use S3). */
function download(url, redirectsLeft = 5) {
  return new Promise((resolve, reject) => {
    https
      .get(url, { headers: { "User-Agent": "brick-npm-installer" } }, (res) => {
        if (
          res.statusCode >= 300 &&
          res.statusCode < 400 &&
          res.headers.location
        ) {
          if (redirectsLeft <= 0) {
            reject(new Error("too many redirects"));
            return;
          }
          res.resume();
          resolve(download(res.headers.location, redirectsLeft - 1));
          return;
        }
        if (res.statusCode !== 200) {
          reject(new Error(`HTTP ${res.statusCode} for ${url}`));
          res.resume();
          return;
        }
        const chunks = [];
        res.on("data", (chunk) => chunks.push(chunk));
        res.on("end", () => resolve(Buffer.concat(chunks)));
      })
      .on("error", reject);
  });
}

async function main() {
  const target = resolveTarget();
  if (!target) {
    console.warn(
      `brick: no prebuilt binary for ${process.platform}/${process.arch}. ` +
        "Build from source (cargo install --path crates/cli) or open an issue."
    );
    // Don't fail the whole npm install; the shim will try PATH.
    process.exit(0);
  }

  const assetName = `brick-${target.triple}${target.ext}`;
  const base = `https://github.com/${REPO}/releases/download/v${VERSION}`;
  const binUrl = `${base}/${assetName}`;
  const shaUrl = `${binUrl}.sha256`;

  const binDir = path.join(__dirname, "..", "bin");
  fs.mkdirSync(binDir, { recursive: true });
  const dest = path.join(binDir, target.isWindows ? "brick-native.exe" : "brick-native");

  console.log(`brick: downloading ${assetName} (v${VERSION})...`);
  let binary;
  try {
    binary = await download(binUrl);
  } catch (error) {
    console.warn(
      `brick: failed to download binary (${error.message}). ` +
        "The `brick` command will fall back to a `brick` on your PATH if present."
    );
    process.exit(0);
  }

  // Verify checksum when the Release publishes a .sha256 sidecar. A mismatch is
  // fatal — we never install an unverified binary.
  try {
    const shaText = (await download(shaUrl)).toString("utf8").trim();
    const expected = shaText.split(/\s+/)[0].toLowerCase();
    const actual = createHash("sha256").update(binary).digest("hex");
    if (expected && expected !== actual) {
      console.error(
        `brick: checksum mismatch for ${assetName} (expected ${expected}, got ${actual}). Aborting.`
      );
      process.exit(1);
    }
  } catch (error) {
    // No sidecar published — proceed without verification but say so.
    console.warn(`brick: checksum not verified (${error.message}).`);
  }

  fs.writeFileSync(dest, binary, { mode: 0o755 });
  console.log(`brick: installed native binary at ${dest}`);
}

main().catch((error) => {
  console.warn(`brick: postinstall error (${error.message}); the shim will try PATH.`);
  process.exit(0);
});
