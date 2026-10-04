# Building Market Workbench from source

## Requirements

- Windows x64, Rust with the `x86_64-pc-windows-msvc` target and MSVC build tools.
- The Rust toolchain must support edition 2024. Cargo.lock pins dependency versions;
  first compilation downloads public crates if they are not cached.
- Python 3.11+ is needed for evidence/packaging scripts and their tests, not for
  running the Workbench executable.

The backend remains in `source/tools/market-seederv3/`. It shares the existing
`source/externalservices/market-server/crates/market-common/` crate; keep this
relative directory structure. Frontend source is in root `workbench-ui/`.

## Compile and test

From the repository root:

```powershell
$repoRoot = (Get-Location).Path
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --manifest-path source/tools/market-seederv3/Cargo.toml --locked --release --target x86_64-pc-windows-msvc
cargo test --manifest-path source/tools/market-seederv3/Cargo.toml --locked
Push-Location source/tools/market-seederv3
python -m unittest test_derive_tq_snapshot test_collect_tq_history test_blend_tq_history
Pop-Location
```

The executable is
`source/tools/market-seederv3/target/x86_64-pc-windows-msvc/release/market-seederv3.exe`.
The portable package calls this executable `market-workbench.exe`. Embedded Rust
unit tests cover policy resolution, sources, Workbench workflows and path safety;
Python tests cover the offline TQ derivation/history tools.

## Required offline runtime data

Compilation does not need the large pinned datasets. Starting Workbench does.
Download the portable ZIP and matching `.sha256` from this repository's Release,
verify the ZIP hash and extract the whole archive into an EveJS installation.
For development, use a separate test installation with gameplay services stopped.

The extracted `tools/marketworkbench/` folder provides:

- `inputs/`: pinned generated EveJS static tables, CCP SDE, TQ trade reference,
  taxonomy, LP catalog, acquisition/reference evidence and the NPC order snapshot.
- `data/`: the legacy pinned price manifest and auxiliary reference inputs.
- `config/`: the two runtime TOML configs and their JSON item policies.
- `workbench-ui/`, `presets/`, `licenses/`, `DATA-SOURCES.md` and
  `package-manifest.json`: UI, community presets, attribution and file identities.

Keep the matching inputs, data, configs and manifests together. The TOML configs
use relative paths and pinned hashes; missing or mismatched data must be reported,
not silently replaced. See root `DATA-SOURCES.md` and the portable manifest for
provenance. Large datasets, build output, user policies and SQLite DBs are excluded
from Git. Do not substitute a live gameplay database as pricing input.

Use your compiled backend with the verified portable runtime inputs by copying
only your build/UI into your own test installation:

```powershell
$eveRoot = 'C:\Your-Test-EveJS'
$workbenchRoot = Join-Path $eveRoot 'tools\marketworkbench'
Copy-Item (Join-Path $repoRoot 'source\tools\market-seederv3\target\x86_64-pc-windows-msvc\release\market-seederv3.exe') (Join-Path $workbenchRoot 'market-workbench.exe')
Copy-Item (Join-Path $repoRoot 'workbench-ui\*') (Join-Path $workbenchRoot 'workbench-ui') -Recurse -Force
Push-Location $workbenchRoot
.\market-workbench.exe workbench --port 8765 --storage-dir user-data --install-root $eveRoot
Pop-Location
```

Wait for `Market Workbench is ready: http://127.0.0.1:8765/` before opening the
browser. Ctrl+C stops Workbench. These steps do not build/install a market DB
and do not start or stop EveJS gameplay services.

## Configs, presets and evidence tooling

Runtime-compatible relative-path TOML examples are under
`source/tools/market-seederv3/config/examples/`; JSON policies and friendly-group
definitions are in its parent directory. Community import/export JSON files are
under root `presets/community-v1/`. Their embedded Item Policy and Distribution
settings are source configuration, not captured trading datasets.

`source/evidence-collector/esi_trade_reference.py` and the TQ scripts under
`source/tools/market-seederv3/` collect or derive offline inputs. Inspect their
`--help` and `doc/TQ-JITA-SNAPSHOT.md` before making a new capture. Workbench
startup does not collect prices from the network.

`source/package_workbench.py` is the corresponding packaging script, not a
one-command reproduction of the release from a bare checkout. It requires pinned
input locations/config and Python as well as a compiled binary. Use the published
portable Release for the exact approved dataset bundle.
