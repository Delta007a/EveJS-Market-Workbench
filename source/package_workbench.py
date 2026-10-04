"""Build a curated Windows Workbench ZIP; never include player state or user presets.

Run after compiling a release EXE. Uses only pinned local inputs and the Cargo cache.
The packaging machine needs Python/Cargo; recipients need neither.
"""
import argparse
import hashlib
import json
import pathlib
import shutil
import subprocess
import tomllib
import zipfile

TABLES = ("stations", "solarSystems", "itemTypes", "industryBlueprints", "typeDogma", "reprocessingStatic")
RAW = ("_sde.jsonl", "types.jsonl", "marketGroups.jsonl", "npcStations.jsonl", "npcCorporations.jsonl", "mapStargates.jsonl")


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def json_text(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2) + "\n"


def toml_text(value):
    lines = []

    def atom(v):
        if isinstance(v, bool):
            return "true" if v else "false"
        if isinstance(v, str):
            return json.dumps(v, ensure_ascii=False)
        if isinstance(v, (int, float)):
            return str(v)
        if isinstance(v, list):
            return "[" + ", ".join(atom(x) for x in v) + "]"
        raise ValueError("Unsupported TOML value")

    def section(table, keys=()):
        if keys:
            lines.append("[" + ".".join(json.dumps(k) for k in keys) + "]")
        for key, v in table.items():
            if not isinstance(v, dict):
                lines.append(json.dumps(key) + " = " + atom(v))
        lines.append("")
        for key, v in table.items():
            if isinstance(v, dict):
                section(v, (*keys, key))

    section(value)
    return "\n".join(lines)


LAUNCHER = r'''@echo off
setlocal
cd /d "%~dp0tools\marketworkbench"
if errorlevel 1 goto missing
if not exist "market-workbench.exe" goto missing
if not exist "%~dp0StartMarketServer.bat" (
  echo Extract this package into your EveJS folder, beside StartMarketServer.bat.
  pause
  exit /b 1
)
set "WB_PORT=8765"
if not "%MARKET_WORKBENCH_PORT%"=="" set "WB_PORT=%MARKET_WORKBENCH_PORT%"
echo Starting Market Workbench. Please wait.
echo Wait for the backend line: Market Workbench is ready: http://127.0.0.1:%WB_PORT%/
echo Open that address in your browser only after the ready line appears.
echo Your presets and market databases: %CD%\user-data
echo Press Ctrl+C here to stop Workbench. EveJS services are not controlled.
if "%~1"=="" (
  "market-workbench.exe" workbench --port "%WB_PORT%" --storage-dir "%CD%\user-data" --install-root "%~dp0."
) else (
  "market-workbench.exe" workbench --port "%WB_PORT%" --storage-dir "%CD%\user-data" --install-root "%~1"
)
set "WB_EXIT=%ERRORLEVEL%"
if not "%WB_EXIT%"=="0" (
  echo.
  echo Workbench exited with code %WB_EXIT%. See the error above.
  echo If the port is already in use, close the previous Workbench or set MARKET_WORKBENCH_PORT.
  pause
)
exit /b %WB_EXIT%
:missing
echo Missing tools\marketworkbench\market-workbench.exe. Extract the complete ZIP first.
pause
exit /b 1
'''

README = '''# Market Workbench — portable Windows package

## English

Windows x64, EveJS 0.12.9 / SDE build 3396210. Extract the **entire ZIP into the EveJS root**:

```
EveJS/
  README-FIRST.txt
  StartMarketWorkbench.bat
  tools/README.md
  tools/marketworkbench/
    market-workbench.exe
    config/                 shipped presets and relative operational paths
    workbench-ui/           browser interface
    inputs/                 pinned TQ, public static/SDE, NPC/LP evidence
    data/price-manifest.json
    user-data/              created locally: policies, drafts, backups, candidates, reports
    source/                 corresponding Rust source and Cargo.lock
    licenses/               license texts and dependency notices
    package-manifest.json   file sizes / SHA256
```

Double-click `StartMarketWorkbench.bat`. Wait for the backend line:

```
Market Workbench is ready: http://127.0.0.1:8765/
```

Then open http://127.0.0.1:8765/. **TQ-like Market — Recommended** is the starting
preset using captured Tranquility prices. **Legacy Market** is the older solo-economy
variant. Edit a copy to customize; built-in presets remain unchanged.

No Rust, Cargo, Python, Node or Internet is needed to run, preview,
save, build or audit. The browser interface is served locally. Ctrl+C stops only Workbench.
The public TQ price snapshot was captured on 2026-09-28; it is pinned, not live TQ pricing.
References blend Jita's highest eligible Buy / lowest station Sell with The Forge trading history, 50/50.
History uses 7 / 30 / 180 / 365 completed-day windows; sparse/missing history is visible.
Buy >= Sell is a review warning. Other hubs do not contribute. GENERAL_TQ's explicit fallbacks remain.

Create/edit a copy of a preset. Save, then **Build Market Database** and **Verify Market Database**. Its location is
`tools/marketworkbench/user-data/candidates/<id>/market.sqlite`. Nothing is installed by
building. **Install into EveJS** verifies the market database, reads the selected installation's
market-server config and copies the DB there after explicit confirmation. Stop the target
EveJS and market-server first. Existing DB is backed up; replacing it resets its player
orders/events and remaining synthetic stock. Workbench never starts/stops gameplay services.

The default installation is this EveJS folder. A different registered destination remains
available through `StartMarketWorkbench.bat "D:\\Games\\EveJS"`; there is no folder picker.
If needed, set `MARKET_WORKBENCH_PORT` before launching (e.g. `set MARKET_WORKBENCH_PORT=8767`).
Do not expose the local backend publicly.

LEGACY_V1 is unchanged. GENERAL_TQ uses the prepared Jita dataset. Special/non-market item opt-ins, Blueprint families,
Distribution and Import/Export are included. No personal presets, accounts, passwords,
player state, candidate database, gameplay config, compiler or build cache is shipped.
Use Export/Import to transfer your presets; old market databases contain local provenance paths
and should be rebuilt on the receiving installation. Keep `user-data` when updating.

All required pricing/catalog data is included. Do not copy only the EXE or remove inputs.
TQ dataset changes require updating its pinned SHA in the operational config, not editing
the price algorithm or using an implicit fallback. This package has no automatic refresh.

The source is included under `tools/marketworkbench/source/`; see its `BUILD.md`.
Licenses and CCP data attribution are under `tools/marketworkbench/licenses/` and
`tools/marketworkbench/DATA-SOURCES.md`. This is an EveJS add-on.

The EXE is unsigned: Windows SmartScreen may warn. The Release includes the ZIP's
`.sha256` file; `tools/marketworkbench/package-manifest.json` lists individual file SHA256.

## Русский

Распакуйте **весь ZIP в корень своей папки EveJS**, рядом с `StartMarketServer.bat`.
Запустите `StartMarketWorkbench.bat`, дождитесь строки
`Market Workbench is ready: http://127.0.0.1:8765/` и откройте этот адрес.
Рекомендуемый старт — **TQ-like Market**. **Legacy Market** — старый вариант экономики.
Rust, Python, Node и Интернет для обычной работы не нужны. Нужна Windows x64 и браузер.

Свои presets и результаты находятся в `tools\\marketworkbench\\user-data`.
При обновлении эту папку сохраняйте. Для переноса presets используйте Export / Import.
Исходные LEGACY_V1 / GENERAL_TQ сохранены; все Advanced-подгруппы и Distribution доступны.
Community presets нужно импортировать кнопкой **Import Preset** из
`tools\\marketworkbench\\presets\\community-v1\\`. Они не загружены заранее.

Сборка создаёт отдельную базу. Установка в EveJS — отдельная кнопка **Install into EveJS**,
после проверки и подтверждения. До установки остановите соответствующие EveJS и Market Server.
Старая база получает backup. Сервисы Workbench сам не запускает и не останавливает.

В архиве есть EXE, интерфейс, configs, все необходимые публичные данные, исходники,
лицензии и список SHA256. Ваших аккаунтов, presets, построенных баз и игрового состояния нет.
Снимок цен TQ датирован 28.09.2026 и автоматически не обновляется.
Цены подготовлены заранее: 50% Jita snapshot + 50% истории торгов The Forge.
Окна истории: 7 / 30 / 180 / 365 дней. Buy >= Sell — предупреждение, не блокировка.
Другие хабы не влияют на цену. Явные fallback-настройки GENERAL_TQ сохранены.
EXE не подписан, поэтому Windows SmartScreen может предупредить.
Рядом с ZIP в Release есть `.sha256`; внутри есть manifest с SHA256 файлов.
'''

README_FIRST = '''MARKET WORKBENCH — START HERE / НАЧНИТЕ ЗДЕСЬ

1) Extract the ENTIRE archive into your EveJS root folder, beside StartMarketServer.bat.
2) Run StartMarketWorkbench.bat.
3) Wait for this exact backend line:
   Market Workbench is ready: http://127.0.0.1:8765/
4) Open http://127.0.0.1:8765/ in your browser.
5) TQ-like Market is the Recommended starting preset. Edit a Copy to customize it.
6) Legacy Market is the older/legacy economy preset.
7) Import community JSON presets with Import Preset from:
   tools\\marketworkbench\\presets\\community-v1\\
8) Build Market Database creates a SEPARATE database. Verify Market Database checks it.
   Nothing is installed into gameplay without your explicit confirmation.
9) The EXE is unsigned; Windows SmartScreen may warn. The Release includes ZIP.sha256.
   Check the actual ZIP filename's .sha256 file. File hashes are in package-manifest.json.

Full guide: tools\\README.md. No Rust/Python/Node installation is required.
Keep tools\\marketworkbench\\user-data when updating. Ctrl+C stops Workbench only.

1) Распакуйте ВЕСЬ архив в корень EveJS, рядом с StartMarketServer.bat.
2) Запустите StartMarketWorkbench.bat.
3) Дождитесь: Market Workbench is ready: http://127.0.0.1:8765/
4) Откройте этот адрес в браузере.
5) TQ-like Market — рекомендуемый старт. Для изменений выберите Edit a Copy.
6) Legacy Market — старый вариант экономики.
7) Community presets импортируйте через Import Preset из
   tools\\marketworkbench\\presets\\community-v1\\.
8) Сначала строится ОТДЕЛЬНАЯ база рынка. В игру она не устанавливается без подтверждения.
9) EXE не подписан: SmartScreen может предупредить. Рядом с ZIP в Release есть .sha256.

Подробная инструкция: tools\\README.md.
'''


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--exe", type=pathlib.Path, required=True)
    parser.add_argument("--zip", type=pathlib.Path, required=True)
    parser.add_argument("--tool-dir", type=pathlib.Path, help="fresh package staging directory")
    parser.add_argument("--launcher", type=pathlib.Path, help="fresh launcher staging path")
    args = parser.parse_args()
    crate = pathlib.Path(__file__).resolve().parents[1]
    evejs = crate.parents[1]
    tool = args.tool_dir or evejs / "tools/marketworkbench"
    if tool.exists():
        raise SystemExit("Package directory already exists; choose a fresh staging copy. User data is never removed.")
    if args.zip.exists():
        raise SystemExit("ZIP already exists; will not replace a release silently.")
    if not args.exe.is_file():
        raise SystemExit("Compile the release EXE first.")
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--offline", "--locked", "--format-version", "1", "--filter-platform", "x86_64-pc-windows-msvc"], cwd=crate))
    legacy = tomllib.loads((crate / "config/market-policy-v2.local.toml").read_text())
    general = tomllib.loads((crate / "config/market-policy-general-tq-v1.local.toml").read_text())
    static_dir = pathlib.Path(legacy["input"]["static_data_dir"])
    source_manifest = static_dir.parent / "manifest.json"
    m = json.loads(source_manifest.read_bytes())
    build = m["build"]
    raw = static_dir.parents[1] / f"sde/eve-online-static-data-{build}-jsonl"
    tq = pathlib.Path(general["policy"]["tq_snapshot_path"])
    expected_tq = general["policy"]["tq_snapshot_sha256"]
    if sha(tq) != expected_tq:
        raise SystemExit("Pinned TQ SHA mismatch")
    snapshots = sorted(pathlib.Path(legacy["source"]["download_dir"]).glob("market-orders-*.csv.*.bz2"))
    if len(snapshots) != 1:
        raise SystemExit("Expected exactly one pinned reference snapshot")
    lp = (crate / legacy["policy"]["lp_offer_catalog_path"]).resolve()
    required = [args.exe, source_manifest, tq, lp, *snapshots]
    required += [static_dir / name / "data.json" for name in TABLES]
    required += [raw / name for name in RAW]
    if not all(p.is_file() for p in required):
        raise SystemExit("Required pinned package input is missing")
    tool.mkdir(parents=True)
    identities = []

    def copy(src, relative, authority=None):
        dest = tool / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src, dest)
        if sha(src) != sha(dest):
            raise RuntimeError(f"Copy SHA mismatch: {relative}")
        if authority:
            identities.append(dict(file=relative, authority=authority, sha256=sha(dest)))

    def write(relative, text):
        dest = tool / relative
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(text, encoding="utf-8", newline="\n")

    copy(args.exe, "market-workbench.exe")
    for name in TABLES:
        copy(static_dir / name / "data.json", f"inputs/_local/gameStore/data/{name}/data.json", "public_generated_static")
    for name in RAW:
        copy(raw / name, f"inputs/_local/sde/eve-online-static-data-{build}-jsonl/{name}", "ccp_sde")
    public_manifest = {k: m[k] for k in ("version", "generatedAt", "build", "sdeUrl", "sdeMeta", "sdeSha256") if k in m}
    public_manifest.update(outputDataDir="data", generatedTables=list(TABLES), sourceManifestSHA256=sha(source_manifest))
    write("inputs/_local/gameStore/manifest.json", json_text(public_manifest))
    copy(tq, "inputs/tq-market-reference.json", "official_esi_derived_tq_trade_reference")
    copy(tq.parent / "manifest.json", "inputs/tq-reference-manifest.json", "offline_snapshot_derivation_manifest")
    copy(tq.parent / "coverage.json", "inputs/tq-reference-coverage.json")
    copy(lp, "inputs/lpStoreStaticOffers.json", "evejs_static_lp_catalog")
    copy(snapshots[0], "inputs/snapshot/" + snapshots[0].name, "pinned_everef_public_npc_reference")
    copy(crate / legacy["price_manifest"]["path"], "data/price-manifest.json", "legacy_pinned_price_manifest")
    taxonomy_path = pathlib.Path(general["policy"]["ordinary_taxonomy_path"])
    taxonomy = json.loads(taxonomy_path.read_bytes())
    # These are the only taxonomy fields consumed by Workbench / GENERAL_TQ.
    projected = {"format_version": 1, "source_sha256": sha(taxonomy_path),
                 "types": [{k: row[k] for k in ("typeID", "semanticClass", "family")} for row in taxonomy["types"]]}
    write("inputs/ordinary-taxonomy.json", json_text(projected))
    for name in ("market-policy-v2.json", "market-policy-general-tq-v1.json", "workbench-friendly-groups-v1.json"):
        copy(crate / "config" / name, "config/" + name)
    for config, name in ((legacy, "market-policy-v2.local.toml"), (general, "market-policy-general-tq-v1.local.toml")):
        config["input"]["static_data_dir"] = "inputs/_local/gameStore/data"
        config["output"]["database_path"] = "user-data/candidates/unused-direct-output.sqlite"
        config.setdefault("source", {})["download_dir"] = "inputs/snapshot"
        config.setdefault("price_manifest", {})["path"] = "data/price-manifest.json"
        config["price_manifest"]["capture_missing_from_snapshot"] = False
        config["price_manifest"]["capture_missing_from_esi"] = False
        config["policy"]["lp_offer_catalog_path"] = "inputs/lpStoreStaticOffers.json"
        config["policy"]["plan_preview_path"] = "user-data/reports/preview.json"
        config["policy"].pop("parity_baseline_path", None)
        if name.startswith("market-policy-general"):
            config["policy"]["tq_snapshot_path"] = "inputs/tq-market-reference.json"
            config["policy"]["ordinary_taxonomy_path"] = "inputs/ordinary-taxonomy.json"
        write("config/" + name, toml_text(config))
    # Curated portable user presets, not existing personal user-data.
    community = crate / "presets/community-v1"
    if community.exists():
        for file in sorted(community.iterdir()):
            if file.is_file():
                copy(file, "presets/community-v1/" + file.name, "curated_portable_user_preset")
    for file in (crate / "workbench-ui").iterdir():
        if file.is_file():
            copy(file, "workbench-ui/" + file.name)
    write("user-data/README.txt", "Local user data is created here. Keep this folder when updating. No personal data is shipped.\n")
    write("DATA-SOURCES.md", "# Pinned public data\n\nSDE build " + str(build) + ". TQ capture 2026-09-28. Industry adjusted price is retained only as informational evidence and is not a TQ trade quote.\n\n" + json_text(identities) + "\nOrdinary taxonomy is a lossless projection of the fields actually consumed by the tool; source SHA: `" + sha(taxonomy_path) + "`. Other static/SDE/pricing inputs are byte-identical copies. Historical manifest local output path was replaced by a relative path. No private account/character/runtime tables were copied.\n")
    copy(evejs / "LICENSE", "licenses/LICENSE")
    for file in (crate / "src").iterdir():
        if file.is_file():
            copy(file, "source/tools/market-seederv3/src/" + file.name)
    for name in ("Cargo.toml", "Cargo.lock"):
        copy(crate / name, "source/tools/market-seederv3/" + name)
    for name in ("derive_tq_snapshot.py", "generate_general_tq.py", "test_derive_tq_snapshot.py", "blend_tq_history.py", "test_blend_tq_history.py", "collect_tq_history.py", "test_collect_tq_history.py"):
        copy(crate / name, "source/tools/market-seederv3/" + name)
    copy(crate / "doc/TQ-JITA-SNAPSHOT.md", "source/tools/market-seederv3/doc/TQ-JITA-SNAPSHOT.md")
    copy(pathlib.Path("G:/EVESP/EveJS-CodexLab/work/pricing-data/trade-reference-v1/collector/esi_trade_reference.py"), "source/evidence-collector/esi_trade_reference.py")
    for name in ("market-policy-v2.json", "market-policy-general-tq-v1.json", "workbench-friendly-groups-v1.json"):
        copy(crate / "config" / name, "source/tools/market-seederv3/config/" + name)
    common = evejs / "externalservices/market-server/crates/market-common"
    copy(common / "Cargo.toml", "source/externalservices/market-server/crates/market-common/Cargo.toml")
    copy(common / "src/lib.rs", "source/externalservices/market-server/crates/market-common/src/lib.rs")
    copy(pathlib.Path(__file__), "source/package_workbench.py")
    write("source/BUILD.md", "# Corresponding source\n\nBuild with Rust/MSVC on Windows x64:\n\n```powershell\ncd tools/market-seederv3\n$env:RUSTFLAGS='-C target-feature=+crt-static'\ncargo build --locked --release --target x86_64-pc-windows-msvc\n```\n\nCargo.lock pins third-party versions; initial compilation downloads those public dependencies if they are not cached. Recipients running the included EXE need no compiler/network. Copy the built market-seederv3.exe to ../../.. / market-workbench.exe. Runtime inputs/config/UI are in the enclosing package. Package generation itself additionally requires the original pinned input locations and Python 3.11+.\n")
    notices = ["# Third-party dependencies\n\nExact versions from Cargo.lock. License texts below accompany the source and binary.\n"]
    for package in sorted(metadata["packages"], key=lambda p: (p["name"], p["version"])):
        if not package["source"]:
            continue
        label = package["name"] + "-" + package["version"]
        root = pathlib.Path(package["manifest_path"]).parent
        notices.append(f"- {label}: {package['license'] or 'see included license file'}; {package.get('repository') or package.get('homepage') or 'Cargo registry package'}")
        license_paths = {p for pattern in ("LICENSE*", "LICENCE*", "COPYING*", "NOTICE*", "COPYRIGHT*") for p in root.glob(pattern) if p.is_file()}
        if package.get("license_file"):
            license_paths.add(root / package["license_file"])
        for p in sorted(license_paths):
            copy(p, "licenses/dependencies/" + label + "/" + p.name)
    write("licenses/THIRD-PARTY-NOTICES.md", "\n".join(notices) + "\n")
    launcher = args.launcher or evejs / "StartMarketWorkbenchPortable.bat"
    if launcher.exists():
        raise RuntimeError("Portable launcher already exists; refusing overwrite")
    launcher.parent.mkdir(parents=True, exist_ok=True)
    launcher.write_bytes(LAUNCHER.replace("\n", "\r\n").encode("ascii"))
    first_readme = launcher.parent / "README-FIRST.txt"
    full_readme = tool.parent / "README.md"
    if first_readme.exists() or full_readme.exists():
        raise RuntimeError("Release onboarding docs already exist; choose a fresh staging copy")
    first_readme.write_text(README_FIRST, encoding="utf-8", newline="\n")
    full_readme.write_text(README + "\n## Six community presets\n\nUse **Import Preset** to import a JSON file from `tools\\marketworkbench\\presets\\community-v1\\`, review the summary and save your user copy. They are not preloaded. See `tools/marketworkbench/presets/community-v1/README.md` for the preset table and descriptions.\n", encoding="utf-8", newline="\n")
    root_paths = {"StartMarketWorkbench.bat": launcher, "README-FIRST.txt": first_readme, "tools/README.md": full_readme}
    files = {p.relative_to(tool).as_posix(): {"bytes": p.stat().st_size, "sha256": sha(p)} for p in sorted(tool.rglob("*")) if p.is_file()}
    manifest = {"format_version": 1, "package": "MarketWorkbench-1.3-portable-win-x64", "platform": "Windows x64", "sde_build": build,
                "tq_snapshot_sha256": expected_tq, "preset_sha256": {name: sha(tool / "config" / name) for name in ("market-policy-v2.json", "market-policy-general-tq-v1.json")},
                "user_data_included": False, "files": files,
                "root_files": {name: {"bytes": path.stat().st_size, "sha256": sha(path)} for name, path in root_paths.items()}}
    write("package-manifest.json", json_text(manifest))
    args.zip.parent.mkdir(parents=True, exist_ok=True)
    with zipfile.ZipFile(args.zip, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        def add(name, data):
            info = zipfile.ZipInfo(name, date_time=(2026, 10, 4, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            archive.writestr(info, data, compresslevel=9)
        for name, path in root_paths.items():
            add(name, path.read_bytes())
        for path in sorted(tool.rglob("*")):
            if path.is_file():
                add("tools/marketworkbench/" + path.relative_to(tool).as_posix(), path.read_bytes())
    archive_sha = sha(args.zip)
    args.zip.with_suffix(args.zip.suffix + ".sha256").write_text(archive_sha + "  " + args.zip.name + "\n", encoding="utf-8")
    result = {"tool_dir": str(tool), "local_launcher": str(launcher), "zip": str(args.zip), "zip_bytes": args.zip.stat().st_size,
              "zip_sha256": archive_sha, "unpacked_bytes": sum(p.stat().st_size for p in tool.rglob("*") if p.is_file()), "files": len(files) + len(root_paths) + 1}
    print(json_text(result))


if __name__ == "__main__":
    main()
