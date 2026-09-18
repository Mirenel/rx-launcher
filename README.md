# Project Rx Launcher

Public source map for the x86_64 launcher. The repository contains the source
and the minimal npm manifests needed to build it; release packaging and
private release material are maintained separately.

## Source map

- `package.json` — npm metadata and the Tauri CLI entry point.
- `package-lock.json` — locked npm dependencies.
- `src/` — frontend HTML, JavaScript, CSS, bundled fonts, and artwork.
- `src-tauri/Cargo.toml` and `Cargo.lock` — Rust package metadata and locked
  dependencies.
- `src-tauri/src/` — Tauri entry point, IPC commands, content and executable
  patch handling, runtime detection, and signed update logic.
- `src-tauri/src/tools/` — optional feature-gated Rust release tools; normal
  launcher builds do not enable them.
- `src-tauri/tauri.conf.json` — shared application and Windows bundle config.
- `src-tauri/tauri.linux.conf.json` — Linux bundle targets and window config.
- `src-tauri/capabilities/` — Tauri permissions.
- `src-tauri/icons/` — application icons.
- `src-tauri/public-keys/` — public verification keys only.

## Supported OS and repositories

All supported targets are x86_64. Wine is an external prerequisite for the
Windows game client on Linux; the launcher does not install it.

| Target | Supported form | Source or package repository |
| --- | --- | --- |
| Windows | Native Tauri app and NSIS initial-install target. | [Launcher releases](https://github.com/Mirenel/rx-launcher/releases); no package repository is defined here. |
| Debian, Ubuntu, Devuan | `.deb` or AppImage. | Standalone release artifacts; no APT repository is defined here. |
| Fedora | `.rpm`. | Standalone release artifacts; no Fedora repository or COPR source is defined here. |
| Arch Linux | Arch-compatible best effort. | No Arch package repository or automatic AUR publication is defined here. |
| CachyOS | Arch-compatible best effort, not native-certified. | No CachyOS repository or CachyOS-specific runtime branch is required. |

The source repository is [Mirenel/rx-launcher](https://github.com/Mirenel/rx-launcher).
Signed game-content manifests and release assets come from
[Mirenel/rx-launcher-content](https://github.com/Mirenel/rx-launcher-content),
with the Project Rx service as the authenticated fallback. The legacy
`rx-patches` repository is not used.
