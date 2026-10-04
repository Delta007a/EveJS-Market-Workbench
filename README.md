# Market Workbench

A local browser tool for creating and configuring synthetic NPC markets for **EveJS 0.12.9**.
Windows x64 portable: no Rust, Python or Node installation is needed.

## Start here

1. **[Download the latest Release](https://github.com/Delta007a/EveJS-Market-Workbench/releases/latest)** — the portable win-x64 ZIP and its `.sha256` file.
2. Extract the **entire ZIP into your EveJS root folder**, beside `StartMarketServer.bat`.
3. Run **`StartMarketWorkbench.bat`**.
4. Wait for this exact backend line:

   ```text
   Market Workbench is ready: http://127.0.0.1:8765/
   ```

5. Open that address in your browser. Choose **TQ-like Market — Recommended**, then **Edit a Copy**.

The short guide is `README-FIRST.txt` beside the launcher. The full guide is `tools/README.md`.
Keep `tools/marketworkbench/user-data` when updating.

## Presets and settings

- **TQ-like Market — Recommended:** practical starting preset using the included pinned Tranquility reference snapshot. Prices are prepared offline; they are not live TQ prices.
- **Legacy Market:** the older solo-economy variant, preserved separately.
- **Six community presets:** Farmer Hardcore, TQ Galaxy, Zero to Hero, Industrialist, Explorer and Solo Progression. Use **Import Preset** to select a JSON file from `tools\marketworkbench\presets\community-v1\`. Review the summary and save a user copy; presets are not preloaded.
- **Import / Export:** one portable JSON preserves Item Policy, Quick Setup / Advanced, exact overrides, Distribution and its seed.
- **Distribution:** choose Jita, five hubs, regional or other eligible NPC station arrangements, with reproducible assortment, stock and local modifiers.

Buy means the NPC buys from you; Sell means it sells to you. Built-in presets remain read-only: edit your own copy.

## Build, verify, then decide

**Build Market Database** creates a separate database under Workbench user storage.
**Verify Market Database** checks it. Neither action installs it into gameplay.
**Install into EveJS** is a separate, explicitly confirmed action. Stop the target EveJS
and market-server first. The previous database is backed up before replacement.
Workbench does not start or stop gameplay services.

The backend binds to `127.0.0.1`; this is a local tool. Ctrl+C stops Workbench.
No accounts, passwords, personal presets, player state or prebuilt market DB are distributed.

## SHA256 and Windows warning

The EXE is **unsigned**, so Windows SmartScreen may warn. Download from this project's
Release and verify the ZIP against its matching `.sha256` asset. On PowerShell:

```powershell
Get-FileHash .\MarketWorkbench-1.3-community-presets-v1-portable-win-x64-final.zip -Algorithm SHA256
```

Compare it with the `.sha256` text. Individual file identities are listed in
`tools/marketworkbench/package-manifest.json`.

## Quick start checklist

**Download the latest Release → extract the entire ZIP into the EveJS root → run StartMarketWorkbench.bat.**
Wait for `Market Workbench is ready: http://127.0.0.1:8765/`, then open that address.
Start with **TQ-like Market — Recommended** and **Edit a Copy**. **Legacy Market** is the older variant.
Import community presets from `tools\marketworkbench\presets\community-v1\`.
Read `README-FIRST.txt` for the short guide or `tools\README.md` for the full instructions.
Building creates a separate database; installing it into gameplay requires explicit confirmation.
The EXE is unsigned, so Windows SmartScreen may warn. Verify the ZIP against its `.sha256` asset.

## Source and attribution

This repository contains the corresponding Rust backend and shared crate, Cargo.toml/Cargo.lock,
`workbench-ui/`, configuration examples, Python scripts/tests and community preset JSON files.
Start with [source/BUILD.md](source/BUILD.md) for compilation and offline runtime setup.

Large pinned pricing/static datasets are excluded from Git history. They are included in the
portable Release under `tools/marketworkbench/inputs/` and `tools/marketworkbench/data/`.
Use the matching Release to obtain these inputs and verify `package-manifest.json` before
running a source-built binary. A source checkout alone does not contain a runnable pricing dataset.
See [DATA-SOURCES.md](DATA-SOURCES.md) and [LICENSE](LICENSE).
EVE Online / Tranquility static and market data are CCP data; this is an independent EveJS add-on.
