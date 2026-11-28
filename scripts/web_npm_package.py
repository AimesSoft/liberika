#!/usr/bin/env python3
"""Stage the npm package from a built browser runtime.

Run after `cargo run -p xtask -- web build`. The staged directory holds the
runtime files npm should carry and a package.json stamped with the Cargo
workspace version; `npm pack` or `npm publish` then runs inside it.
"""
import argparse
import json
from pathlib import Path
import re
import shutil

ROOT = Path(__file__).resolve().parents[1]
REPOSITORY = 'https://github.com/AimesSoft/liberika'
RUNTIME = ['index.js', 'index.d.ts', 'worker.js', 'renderer.js', 'renderer_webgpu.js', 'source.js', 'audio.js', 'codecs.js',
           'erika_core.js', 'erika_core.d.ts', 'erika_core_bg.wasm', 'erika_core_bg.wasm.d.ts',
           'media.js', 'media.wasm']
DOCUMENTS = ['README.md', 'LICENSE', 'WEB-GUIDE.md', 'WEB-GUIDE.zh.md']
NOFONT_NOTE = ('> This is the build without the fallback font. Hand one over with\n'
               '> `Player.create({ canvas, fallbackFont: { data, index } })`, or install\n'
               '> `@aimessoft/liberika-web`, which carries its own.\n\n')


def workspace_version():
    manifest = (ROOT / 'Cargo.toml').read_text()
    section = manifest.split('[workspace.package]', 1)[1].split('\n[', 1)[0]
    return re.search(r'^version\s*=\s*"([^"]+)"', section, re.M)[1]


def stage(source, out, version, without_fallback_font):
    missing = [name for name in RUNTIME + DOCUMENTS if not (source / name).is_file()]
    if missing or not (source / 'licenses').is_dir():
        raise SystemExit(f'{source} is not a complete browser build; missing {", ".join(missing) or "licenses/"}')
    shutil.rmtree(out, ignore_errors=True)
    out.mkdir(parents=True)
    for name in RUNTIME + DOCUMENTS:
        shutil.copy2(source / name, out / name)
    shutil.copytree(source / 'licenses', out / 'licenses')
    package = json.loads((ROOT / 'packages/erika_web/package.json').read_text())
    package['version'] = version
    readme = (out / 'README.md').read_text()
    # npm resolves a relative link against the repository, where the guides
    # live elsewhere.
    for guide, page in [('WEB-GUIDE.md', 'web.md'), ('WEB-GUIDE.zh.md', 'web.zh.md')]:
        readme = readme.replace(f']({guide})', f']({REPOSITORY}/blob/v{version}/docs/{page})')
    if without_fallback_font:
        package['name'] += '-nofont'
        package['description'] += '; without the fallback font'
        heading, body = readme.split('\n', 1)
        readme = f'{heading}\n\n{NOFONT_NOTE}{body.lstrip()}'
    (out / 'README.md').write_text(readme)
    (out / 'package.json').write_text(json.dumps(package, indent=2) + '\n')
    return package


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, default=ROOT / 'target/web', help='a built browser runtime')
    parser.add_argument('--out', type=Path, help='defaults to target/npm/web or target/npm/web-nofont')
    parser.add_argument('--version', default=workspace_version(), help='defaults to the Cargo workspace version')
    parser.add_argument('--without-fallback-font', action='store_true',
                        help='the runtime was built without the font; stage it as @aimessoft/liberika-web-nofont')
    args = parser.parse_args()
    out = args.out or ROOT / 'target/npm' / ('web-nofont' if args.without_fallback_font else 'web')
    package = stage(args.source, out, args.version, args.without_fallback_font)
    print(f'{package["name"]}@{package["version"]}: {out}', flush=True)


if __name__ == '__main__':
    main()
