# Source build

Windows x64 requires an edition-2024 Rust toolchain and MSVC build tools.
Python 3.11+ is needed only for the Python scripts/tests.

From this **MarketWorkbench** directory:

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --locked --release --target x86_64-pc-windows-msvc
cargo test --locked
Push-Location scripts
python -m unittest test_derive_tq_snapshot test_collect_tq_history test_blend_tq_history
Pop-Location
```

Cargo.lock pins dependencies. Initial compilation downloads public crates if not cached.
The internal Cargo package name remains `market-seederv3` for compatibility with
the existing engine; the directory structure and public launcher are MarketWorkbench.
Copy `target/x86_64-pc-windows-msvc/release/market-seederv3.exe` to
`market-workbench.exe` in this directory.

## Offline inputs

Compilation does not require the pinned datasets; starting Workbench does.
Download and verify the matching portable Release. Copy its `inputs/`, `data/`,
`licenses/` and package manifest into this source directory, keeping the supplied
runtime configs and pinned hashes together. The source checkout already contains
the same JSON presets/configs and browser UI. Missing inputs must not be replaced
silently. Read DATA-SOURCES.md and verify the portable manifest.

Place the resulting MarketWorkbench folder in **EveJS/tools/** and run its
StartMarketWorkbench.bat. Use a separate test EveJS installation for development.
This workflow does not create/install a market DB or control gameplay services.

## Layout

- `src/`: authoritative Rust policy/pricing/Workbench implementation.
- `crates/market-common/`: shared types, referenced by a relative Cargo path.
- `config/`: unchanged built-in policies and portable operational TOML configs.
- `config/fixtures/`: legacy regression fixtures; not active runtime presets.
- `workbench-ui/`: static frontend.
- `scripts/`: evidence derivation, history collection, Python tests and packaging.
- `presets/community-v1/`: community import/export JSONs.

Large datasets, binaries, target output, user-data and SQLite databases are excluded
from Git. The existing pricing logic and preset economics are unchanged.

To assemble the same single-folder layout from an existing verified portable bundle:

```powershell
python scripts/package_workbench.py --runtime-dir C:/VerifiedBundle/MarketWorkbench --exe market-workbench.exe --zip C:/Output/MarketWorkbench-custom.zip
```

The packager verifies its runtime inputs against the original package manifest,
excludes user-data and build output, refuses to overwrite an existing ZIP, and
writes a matching .sha256 sidecar. It does not refresh prices or deploy anything.
Evidence scripts are separate tools; Workbench startup does not collect prices.
