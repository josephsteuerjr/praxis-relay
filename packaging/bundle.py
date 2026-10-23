"""Package already built native binaries; no auth/runtime data enters a bundle."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tarfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent
VERSION = tomllib.loads((ROOT/'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
IDENTIFIER = json.loads((ROOT/'desktop/tauri.conf.json').read_text(encoding='utf-8'))['identifier']
DOCS = ['README.md', 'ПАМЯТКА.md', 'LICENSE', 'LICENSE-APACHE', 'NOTICE', 'packaging/README.md']

def run(*args):
    return subprocess.check_output(list(map(str,args)), encoding='utf-8').strip()

def copy(source, destination, executable=False):
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    os.chmod(destination, 0o755 if executable else 0o644)

def passport(directory, target, commit):
    entries = []
    for path in sorted(directory.rglob('*')):
        if path.is_file() and not path.is_symlink() and path.name != 'BUILD-PASSPORT.json':
            entries.append(dict(name=path.relative_to(directory).as_posix(), bytes=path.stat().st_size,
                                sha256=hashlib.sha256(path.read_bytes()).hexdigest()))
    value = dict(version=VERSION, commit=commit, target=target, files=entries)
    (directory/'BUILD-PASSPORT.json').write_text(json.dumps(value, indent=2, ensure_ascii=False)+'\n',encoding='utf-8')
    return value

def docs(destination):
    for name in DOCS:
        filename = 'PLATFORMS.md' if name.startswith('packaging/') else name
        copy(ROOT/name, destination/filename)

def headless(binary, out, target, commit):
    stage = out/('praxis-relay-'+VERSION+'-'+target+'-headless')
    stage.mkdir(parents=True,exist_ok=True)
    copy(binary,stage/'praxis-relay-headless',True)
    docs(stage)
    passport(stage,target,commit)
    archive=out/(stage.name+'.tar.gz')
    with tarfile.open(archive,'w:gz') as bundle:
        bundle.add(stage,arcname=stage.name)
    return archive

def linux(args):
    assert os.uname().sysname == 'Linux'
    out=args.out;out.mkdir(parents=True,exist_ok=True)
    stage=out/'linux-root';stage.mkdir(exist_ok=True)
    for name in ('praxis-relay-desktop','praxis-relay-headless'):
        copy(args.binaries/name,stage/'usr/bin'/name,True)
    entry=stage/f'usr/share/applications/{IDENTIFIER}.desktop'
    entry.parent.mkdir(parents=True,exist_ok=True)
    entry.write_text(f'[Desktop Entry]\nType=Application\nName=praxis-relay\nComment=Local subscription API and account manager\nExec=praxis-relay-desktop\nIcon={IDENTIFIER}\nTerminal=false\nCategories=Development;Utility;\nStartupNotify=true\nStartupWMClass=praxis-relay-desktop\n',encoding='utf-8')
    copy(ROOT/'desktop/icons/icon.png',stage/f'usr/share/icons/hicolor/256x256/apps/{IDENTIFIER}.png')
    copy(ROOT/'desktop/icons/icon.svg',stage/f'usr/share/icons/hicolor/scalable/apps/{IDENTIFIER}.svg')
    docdir=stage/'usr/share/doc/praxis-relay';docs(docdir)
    passport(docdir,'x86_64-unknown-linux-gnu',args.commit)
    # Record executable hashes too: the installed passport is outside /usr/bin.
    manifest=docdir/'BUILD-PASSPORT.json'
    value=json.loads(manifest.read_text(encoding='utf-8'))
    value['binaries']=[dict(path='/usr/bin/'+n,sha256=hashlib.sha256((stage/'usr/bin'/n).read_bytes()).hexdigest()) for n in ('praxis-relay-desktop','praxis-relay-headless')]
    manifest.write_text(json.dumps(value,indent=2,ensure_ascii=False)+'\n',encoding='utf-8')
    if args.format in ('deb','both'):
        control=stage/'DEBIAN/control';control.parent.mkdir(exist_ok=True)
        control.write_text(f'Package: praxis-relay\nVersion: {VERSION}\nSection: devel\nPriority: optional\nArchitecture: amd64\nMaintainer: Yegor Kosyrev <y.kosyrev@mailfence.com>\nDepends: libc6 (>= 2.36), libgcc-s1, libstdc++6, libssl3, libgtk-3-0, libwebkit2gtk-4.1-0, libayatana-appindicator3-1, libglib2.0-bin, ca-certificates\nRecommends: python3 (>= 3.10), xdg-utils\nHomepage: https://github.com/josephsteuerjr/praxis-relay\nDescription: Local subscription API with native account controls\n Shared Rust headless service and frameless Tauri desktop.\nInstalled-Size: {sum(p.stat().st_size for p in stage.rglob("*") if p.is_file())//1024}\n',encoding='utf-8')
        run('dpkg-deb','--root-owner-group','--build',stage,out/f'praxis-relay_{VERSION}_amd64.deb')
    if args.format in ('rpm','both'):
        top=out/'rpmbuild'
        for folder in ('BUILD','BUILDROOT','RPMS','SOURCES','SPECS','SRPMS'):(top/folder).mkdir(parents=True,exist_ok=True)
        payload=top/'SOURCES/payload.tar.gz'
        with tarfile.open(payload,'w:gz') as bundle:bundle.add(stage/'usr',arcname='usr')
        spec=top/'SPECS/praxis-relay.spec'
        spec.write_text(f'''Name: praxis-relay
Version: {VERSION}
Release: 1
Summary: Local subscription API with native account controls
License: MIT AND Apache-2.0 AND OFL-1.1
URL: https://github.com/josephsteuerjr/praxis-relay
Source0: payload.tar.gz
BuildArch: x86_64
Requires: gtk3, webkit2gtk4.1, libayatana-appindicator-gtk3, glib2, openssl-libs, ca-certificates
Recommends: python3 >= 3.10, xdg-utils
%global debug_package %{{nil}}
%global __os_install_post %{{nil}}
%description
Shared Rust headless service and frameless Tauri desktop. Per-user data is
preserved across package upgrades and removals. No system service is started.
%prep
%build
%install
mkdir -p %{{buildroot}}
tar -xzf %{{SOURCE0}} -C %{{buildroot}}
%files
/usr/bin/praxis-relay-desktop
/usr/bin/praxis-relay-headless
/usr/share/applications/{IDENTIFIER}.desktop
/usr/share/icons/hicolor/256x256/apps/{IDENTIFIER}.png
/usr/share/icons/hicolor/scalable/apps/{IDENTIFIER}.svg
%doc /usr/share/doc/praxis-relay
''',encoding='utf-8')
        run('rpmbuild','--define','_topdir '+str(top.resolve()),'-bb',spec)
        rpms=list((top/'RPMS/x86_64').glob('*.rpm'));assert len(rpms)==1
        copy(rpms[0],out/f'praxis-relay-{VERSION}-1.x86_64.rpm')
    headless(args.binaries/'praxis-relay-headless',out,'linux-x64',args.commit)

def mac(args):
    assert os.uname().sysname == 'Darwin'
    out=args.out;out.mkdir(parents=True,exist_ok=True)
    app=out/'praxis-relay.app';contents=app/'Contents'
    for name in ('praxis-relay-desktop','praxis-relay-headless'):
        copy(args.binaries/name,contents/'MacOS'/name,True)
    copy(ROOT/'desktop/icons/icon.icns',contents/'Resources/icon.icns')
    docs(contents/'Resources/docs')
    metadata=dict(CFBundleName='praxis-relay',CFBundleDisplayName='praxis-relay',
                  CFBundleIdentifier=IDENTIFIER,CFBundleExecutable='praxis-relay-desktop',
                  CFBundlePackageType='APPL',CFBundleIconFile='icon.icns',
                  CFBundleShortVersionString=VERSION,CFBundleVersion=VERSION,
                  LSMinimumSystemVersion='13.3',NSHighResolutionCapable=True,
                  NSPrincipalClass='NSApplication',LSApplicationCategoryType='public.app-category.developer-tools')
    with (contents/'Info.plist').open('wb') as stream:plistlib.dump(metadata,stream)
    # Sign nested executables before sealing the finished app, never alter it later.
    for binary in (contents/'MacOS').iterdir():run('codesign','--force','--sign','-',binary)
    value=passport(contents/'Resources','universal-apple-darwin',args.commit)
    value['binaries']=[dict(path='Contents/MacOS/'+p.name,sha256=hashlib.sha256(p.read_bytes()).hexdigest(),
                           architectures=run('lipo','-archs',p).split()) for p in sorted((contents/'MacOS').iterdir())]
    (contents/'Resources/BUILD-PASSPORT.json').write_text(json.dumps(value,indent=2,ensure_ascii=False)+'\n',encoding='utf-8')
    run('codesign','--force','--sign','-',app)
    run('codesign','--verify','--deep','--strict',app)
    for binary in (contents/'MacOS').iterdir():
        architectures=set(run('lipo','-archs',binary).split())
        assert architectures=={'arm64','x86_64'},architectures
        for line in run('otool','-L',binary).splitlines():
            if line.startswith('\t'):
                library=line.strip().split(' (',1)[0]
                assert library.startswith(('/usr/lib/','/System/Library/')),library
    archive=out/f'praxis-relay-{VERSION}-macos-universal.zip'
    run('ditto','-c','-k','--sequesterRsrc','--keepParent',app,archive)
    image=out/'dmg-root';image.mkdir(exist_ok=True)
    run('ditto',app,image/app.name)
    link=image/'Applications'
    if not link.exists():link.symlink_to('/Applications')
    (image/'READ-ME.txt').write_text('Drag praxis-relay.app to Applications.\nmacOS 13.3+; Intel and Apple Silicon.\nAd-hoc signed; not Apple notarized. First opening may require approval in Privacy & Security.\nAccounts remain in ~/Library/Application Support/praxis-relay.\n',encoding='utf-8')
    run('hdiutil','create','-volname','praxis-relay '+VERSION,'-srcfolder',image,'-ov','-format','UDZO',out/f'praxis-relay-{VERSION}-macos-universal.dmg')
    headless(contents/'MacOS/praxis-relay-headless',out,'macos-universal',args.commit)

def windows(args):
    out=args.out;out.mkdir(parents=True,exist_ok=True)
    for name in ('praxis-relay-desktop.exe','praxis-relay-headless.exe'):
        copy(args.binaries/name,out/name,True)
    docs(out);passport(out,'x86_64-pc-windows-msvc',args.commit)

if __name__=='__main__':
    parser=argparse.ArgumentParser()
    parser.add_argument('platform',choices=['windows','linux','mac'])
    parser.add_argument('--out',type=Path,required=True)
    parser.add_argument('--binaries',type=Path,required=True)
    parser.add_argument('--commit',default=os.environ.get('GITHUB_SHA'))
    parser.add_argument('--format',choices=['deb','rpm','both'],default='both')
    args=parser.parse_args()
    args.commit=args.commit or run('git','-C',ROOT,'rev-parse','HEAD')
    args.out=args.out.resolve();args.binaries=args.binaries.resolve()
    {'windows':windows,'linux':linux,'mac':mac}[args.platform](args)
