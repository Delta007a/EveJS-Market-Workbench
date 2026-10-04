"""Package one MarketWorkbench directory from source and verified offline inputs.

Does not collect evidence, change presets, include user-data, or deploy a database.
"""
from pathlib import Path
import argparse
import hashlib
import json
import shutil
import zipfile


def sha(path):
    with path.open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--runtime-dir', type=Path, required=True)
    parser.add_argument('--exe', type=Path, help='Use a source-built executable instead of the verified portable one')
    parser.add_argument('--zip', type=Path, required=True)
    parser.add_argument('--stage', type=Path, help='Fresh staging directory; default is ZIP name without extension')
    args = parser.parse_args()
    source = Path(__file__).resolve().parents[1]
    runtime = args.runtime_dir.resolve(strict=True)
    stage = (args.stage or args.zip.with_suffix('')).resolve()
    if args.zip.exists() or Path(str(args.zip)+'.sha256').exists() or stage.exists():
        raise SystemExit('Output already exists; refusing to overwrite package/user data')
    original = json.loads((runtime / 'package-manifest.json').read_text(encoding='utf-8-sig'))
    runtime_files = []
    for folder in ('inputs', 'data', 'licenses'):
        for path in sorted((runtime / folder).rglob('*')):
            if path.is_file():
                relative = path.relative_to(runtime).as_posix()
                entry = original['files'].get(relative)
                if entry is None or path.stat().st_size != entry['bytes'] or sha(path) != entry['sha256']:
                    raise SystemExit('Runtime input verification failed: '+relative)
                if path.is_symlink():
                    raise SystemExit('Runtime symlinks are not permitted: '+relative)
                runtime_files.append((path, relative))
    exe = args.exe.resolve(strict=True) if args.exe else runtime / 'market-workbench.exe'
    if not args.exe:
        entry = original['files']['market-workbench.exe']
        if exe.stat().st_size != entry['bytes'] or sha(exe) != entry['sha256']:
            raise SystemExit('Portable executable identity mismatch')
    app = stage / 'MarketWorkbench'
    app.mkdir(parents=True)

    def copy(src, relative):
        target = app / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src, target)

    for folder in ('src', 'crates', 'config', 'workbench-ui', 'presets', 'scripts', 'doc'):
        for path in sorted((source / folder).rglob('*')):
            if path.is_file() and '__pycache__' not in path.parts:
                if path.is_symlink():
                    raise SystemExit('Source symlink is not permitted: '+str(path))
                copy(path, path.relative_to(source))
    for name in ('Cargo.toml', 'Cargo.lock', 'StartMarketWorkbench.bat', 'README.md', 'BUILD.md', 'LICENSE', 'DATA-SOURCES.md'):
        copy(source / name, name)
    copy(exe, 'market-workbench.exe')
    for path, relative in runtime_files:
        copy(path, relative)
    for name in ('policies', 'drafts', 'backups', 'candidates', 'reports'):
        (app / 'user-data' / name).mkdir(parents=True)
    identities = {}
    for path in sorted(app.rglob('*')):
        if path.is_file():
            relative = path.relative_to(app).as_posix()
            identities[relative] = {'bytes': path.stat().st_size, 'sha256': sha(path)}
    manifest = {'format': 'evejs-market-workbench-portable', 'format_version': 1,
                'release': '1.3.1', 'layout_version': 2,
                'installation': 'EveJS/tools/MarketWorkbench', 'files': identities}
    (app / 'package-manifest.json').write_text(json.dumps(manifest, sort_keys=True, indent=2)+'\n', encoding='utf-8')
    args.zip.parent.mkdir(parents=True, exist_ok=True)
    # A partial output never takes the final archive filename.
    temporary = args.zip.with_name(args.zip.name+'.partial')
    if temporary.exists():
        raise SystemExit('Partial ZIP already exists; inspect it before continuing')
    with zipfile.ZipFile(temporary, 'w', zipfile.ZIP_DEFLATED, compresslevel=6) as archive:
        for path in sorted(app.rglob('*')):
            if path.is_file():
                info = zipfile.ZipInfo(path.relative_to(stage).as_posix(), date_time=(2026, 10, 4, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                info.external_attr = 0o100644 << 16
                archive.writestr(info, path.read_bytes(), compresslevel=6)
        for directory in sorted((app / 'user-data').iterdir()):
            info = zipfile.ZipInfo(directory.relative_to(stage).as_posix()+'/', date_time=(2026, 10, 4, 0, 0, 0))
            info.external_attr = (0o40755 << 16) | 0x10
            archive.writestr(info, b'')
    temporary.rename(args.zip)
    digest = sha(args.zip)
    Path(str(args.zip)+'.sha256').write_text(digest+'  '+args.zip.name+'\n', encoding='ascii')
    print(json.dumps({'zip': str(args.zip), 'sha256': digest, 'bytes': args.zip.stat().st_size,
                      'stage': str(stage), 'files': len(identities), 'verified_runtime_files': len(runtime_files)}))


if __name__ == '__main__':
    main()
