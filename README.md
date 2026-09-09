# Project Rx Launcher source

Simple source map. Paths are relative to this repository.

- [`src/`](src/) — Frontend interface
  - [`index.html`](src/index.html) — Page structure
  - [`app.js`](src/app.js) — Interface behavior and launcher actions
  - [`app.css`](src/app.css) and [`fonts.css`](src/fonts.css) — Styling
- [`src-tauri/src/`](src-tauri/src/) — Rust application code
  - [`lib.rs`](src-tauri/src/lib.rs) — Main launcher commands and game operations
  - [`content.rs`](src-tauri/src/content.rs) — Content manifests and downloads
  - [`exe_patch.rs`](src-tauri/src/exe_patch.rs) — Game executable patching
  - [`update.rs`](src-tauri/src/update.rs) — Launcher update flow
  - [`bin/rx-updater.rs`](src-tauri/src/bin/rx-updater.rs) — Updater source
  - [`bin/`](src-tauri/src/bin/) — Supporting patch and signing tools
- [`src-tauri/tauri.conf.json`](src-tauri/tauri.conf.json) — Tauri application configuration
- [`src-tauri/capabilities/`](src-tauri/capabilities/) — Application permissions
- [`src-tauri/icons/`](src-tauri/icons/) — Application icons
- [`src/fonts/`](src/fonts/) and [`src/images/`](src/images/) — Frontend assets
- [`package.json`](package.json) and [`src-tauri/Cargo.toml`](src-tauri/Cargo.toml) — Project dependencies
