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

## Русский — быстрый старт

**Скачать последний Release → распаковать весь ZIP в корень EveJS → запустить StartMarketWorkbench.bat.**
Дождитесь `Market Workbench is ready: http://127.0.0.1:8765/`, затем откройте этот адрес.
Начните с **TQ-like Market — Recommended** и **Edit a Copy**. **Legacy Market** — старый вариант.
Community presets импортируются из `tools\marketworkbench\presets\community-v1\`.
Короткая инструкция — `README-FIRST.txt`, полная — `tools\README.md`.
База строится отдельно; установка в игру требует отдельного подтверждения.
EXE не подписан: SmartScreen может предупредить. Рядом с ZIP есть SHA256.

## Source and attribution

Corresponding Rust source, Cargo.lock, frontend source and community preset JSON files are included in the portable ZIP.
The ZIP also includes the LICENSE, source build guide and data attribution.
Large pinned pricing/static inputs are distributed in Release assets, not Git history.
EVE Online / Tranquility static and market data are CCP data; this is an independent EveJS add-on.
