# Project Rx Launcher source map

This repository publishes the launcher source and the minimal npm manifests
needed to build it.

- `src/` contains the frontend application.
- `src-tauri/` contains the Tauri configuration, Rust application, icons,
  capabilities, and public verification keys.
- `package.json` declares the Tauri CLI build entry point.
- `package-lock.json` pins the npm dependency graph.
