# Market Workbench — user guide

## Installation

Download the portable ZIP and matching `.sha256` from GitHub Releases.
Extract the entire **MarketWorkbench** folder into **tools** inside your EveJS installation:

```text
EveJS/
  tools/
    MarketWorkbench/
      StartMarketWorkbench.bat
      README.md
      market-workbench.exe
      config/
      inputs/
      data/
      workbench-ui/
      presets/
      user-data/
```

Run **StartMarketWorkbench.bat from this folder**. Wait for:

```text
Market Workbench is ready: http://127.0.0.1:8765/
```

Open that address in your browser. Ctrl+C in the Workbench console stops it.
The launcher does not start or stop EveJS or market-server.
For a different local port, set `MARKET_WORKBENCH_PORT` before running it.

## First preset

- **TQ-like Market — Recommended:** uses the included offline Tranquility reference snapshot.
- **Legacy Market:** the older solo-economy preset, preserved separately.
- Select **Edit a Copy** to customize; built-in presets stay read-only.
- Buy means the NPC buys from you. Sell means the NPC sells to you.

Use Quick Setup / Advanced, item overrides and Distribution to configure your market.
Import/Export preserves Item Policy, Quick Setup, exact overrides, Distribution and seed.
Import the six community presets from **presets\community-v1\** using **Import Preset**.
See that directory's README for their descriptions.

## Build, verify, install

Save your preset, select **Build Market Database**, then **Verify Market Database**.
Builds are separate files in `user-data/candidates/`; your gameplay market is unchanged.
**Install into EveJS** is a separate, explicitly confirmed action. Stop the target market-server and close database viewers first. EveJS itself
can remain running; its market is unavailable while market-server is stopped. The previous market DB is backed up before replacement.
Neither the build nor verification automatically installs anything.

Version 1.3.2 handles leftover SQLite WAL/SHM after a stopped server. Installation
first saves the complete database and journal, checks integrity and lets SQLite
finalize committed data. It also keeps a standalone previous-database backup.
Active database users still block installation; Workbench never stops them.

## Updates and Windows

Keep `user-data/` when updating. For the old 1.3 layout, copy your existing
`tools/marketworkbench/user-data/` into this folder's `user-data/` before starting.
Do not replace an existing user-data folder without keeping a backup.

The EXE is unsigned, so SmartScreen may warn. Verify the ZIP against its `.sha256`
asset. `package-manifest.json` lists the identities of individual package files.
Prices are pinned/offline, not a live TQ feed. See DATA-SOURCES.md for provenance.

## Source

Cargo.toml / Cargo.lock, Rust `src/`, shared `crates/`, `scripts/`, tests,
configs and frontend source are included. See BUILD.md.
The GitHub source checkout excludes the EXE and large runtime datasets; obtain
them from the matching portable Release. No user data or gameplay DB is distributed.
