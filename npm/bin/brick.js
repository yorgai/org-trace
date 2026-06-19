#!/usr/bin/env node
"use strict";

// Launcher shim for the Brick CLI installed via npm.
//
// postinstall.js downloads the native binary to bin/brick-native. This shim
// execs it, forwarding all args, stdio, and the exit code. If the native binary
// is missing (download skipped or failed), it falls back to a `brick` on PATH so
// the command still works for users who built from source.

const { spawnSync } = require("child_process");
const fs = require("fs");
const path = require("path");

const isWindows = process.platform === "win32";
const nativeName = isWindows ? "brick-native.exe" : "brick-native";
const nativePath = path.join(__dirname, nativeName);

const target = fs.existsSync(nativePath) ? nativePath : "brick";

const result = spawnSync(target, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  if (result.error.code === "ENOENT") {
    console.error(
      "brick: native binary not found and no `brick` on PATH. " +
        "Reinstall the package or build from source (cargo install --path crates/cli)."
    );
    process.exit(127);
  }
  console.error(`brick: failed to launch (${result.error.message}).`);
  process.exit(1);
}

process.exit(result.status === null ? 1 : result.status);
