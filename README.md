# Market Workbench

Local browser-based NPC market configuration for **EveJS 0.12.9**, Windows x64.

## Download and start

1. [Download the latest portable Release](https://github.com/Delta007a/EveJS-Market-Workbench/releases/latest) and its `.sha256` file.
2. Extract the **MarketWorkbench** folder into your **EveJS\tools** folder.
3. Run **EveJS\tools\MarketWorkbench\StartMarketWorkbench.bat**.
4. Wait for `Market Workbench is ready: http://127.0.0.1:8765/`, then open that address.
5. Start with **TQ-like Market — Recommended**, then **Edit a Copy**.

No Rust, Python or Node installation is needed for the portable version.
The EXE is unsigned; verify the ZIP's SHA256 if SmartScreen warns.

The project lives in one directory: **[MarketWorkbench/](MarketWorkbench/)**.

- [User guide](MarketWorkbench/README.md)
- [Source build instructions](MarketWorkbench/BUILD.md)
- [Community presets](MarketWorkbench/presets/community-v1/)
- [Data attribution](MarketWorkbench/DATA-SOURCES.md)

Build and Verify create/check a separate market database. Installing it into EveJS
requires explicit confirmation. Workbench does not start or stop gameplay services.

The previous 1.3 community-presets Release keeps its original archive layout;
the single-folder layout starts with **1.3.1**. Keep your old `user-data` folder
when updating and copy it into the new MarketWorkbench folder to retain your presets.

## 1.3.2 installer fix

Stopped servers can leave SQLite journals behind. Workbench now backs up and
safely finalizes this state during explicitly confirmed installation. Active
servers/database viewers still block replacement. Pricing, presets and data are unchanged.

## Maintainer release

After merging a PR and getting a green **CI** check, open **Actions → Build Portable Release → Run workflow**.
Enter only the new version, for example `1.4.0`. GitHub builds the Windows portable package, verifies its SHA256,
creates tag `v1.4.0`, and publishes the Release automatically.
