#!/usr/bin/env python3
"""Packs dist/setup.exe out of the release build, using IExpress.

IExpress is the self-extracting package builder that has shipped with Windows
forever. The payload is one exe and two scripts, so there is no reason to add
an installer framework to the build just for that; `cargo build --release`
remains the only thing here that needs a toolchain.

    cargo build --release
    python tools/make_setup.py

Leaves dist/carasoul.exe (the portable build) and dist/setup.exe behind.
"""

import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RELEASE = ROOT / "target" / "release" / "carasoul.exe"
PAYLOAD = Path(__file__).resolve().parent / "setup"
DIST = ROOT / "dist"
FILES = ["carasoul.exe", "install.cmd", "uninstall.cmd"]


def sed_text(target: Path, stage: Path) -> str:
    """The recipe IExpress builds the package from, as a .sed file.

    The quiet commands matter: running the package with `/Q` makes wextract use
    `UserQuietInstCmd`/`AdminQuietInstCmd` and **not** `AppLaunched`, so a
    package that only sets the latter fails with "file not found" the moment
    anyone passes /Q. All three point at the same script here.
    """
    names = "\n".join(f'FILE{i}="{name}"' for i, name in enumerate(FILES))
    sources = "\n".join(f"%FILE{i}%=" for i in range(len(FILES)))
    return f"""[Version]
Class=IEXPRESS
SEDVersion=3
[Options]
PackagePurpose=InstallApp
ShowInstallProgramWindow=0
HideExtractAnimation=1
UseLongFileName=1
InsideCompressed=0
CAB_FixedSize=0
CAB_ResvCodeSigning=0
RebootMode=N
InstallPrompt=
DisplayLicense=
FinishMessage=
TargetName={target}
FriendlyName=carasoul
AppLaunched=%AppLaunched%
UserQuietInstCmd=%UserQuietInstCmd%
AdminQuietInstCmd=%AdminQuietInstCmd%
SourceFiles=SourceFiles

[Strings]
AppLaunched=cmd /c install.cmd
UserQuietInstCmd=cmd /c install.cmd
AdminQuietInstCmd=cmd /c install.cmd
{names}

[SourceFiles]
SourceFiles0={stage}\\

[SourceFiles0]
{sources}
"""


def crlf(src: Path, dst: Path) -> None:
    """Copies a batch file with CRLF endings, whatever the checkout gave us.

    cmd.exe misparses LF-only batch files (multi-line blocks especially), and
    this script packs whatever is in the working tree, so normalise here rather
    than trusting the checkout.
    """
    data = src.read_bytes().replace(b"\r\n", b"\n").replace(b"\n", b"\r\n")
    dst.write_bytes(data)


def main() -> int:
    if not RELEASE.is_file():
        print(
            "target/release/carasoul.exe is missing: run `cargo build --release` first",
            file=sys.stderr,
        )
        return 1

    DIST.mkdir(exist_ok=True)
    shutil.copy2(RELEASE, DIST / "carasoul.exe")

    # IExpress is happiest with a short path that has no spaces in it, so stage
    # the payload in the temp folder rather than in the repo.
    stage = Path(tempfile.gettempdir()) / "carasoul-package"
    shutil.rmtree(stage, ignore_errors=True)
    stage.mkdir()
    shutil.copy2(RELEASE, stage / "carasoul.exe")
    for name in FILES[1:]:
        crlf(PAYLOAD / name, stage / name)

    out = stage / "setup.exe"
    sed = stage / "carasoul.sed"
    sed.write_text(sed_text(out, stage), encoding="ascii")
    subprocess.run(["iexpress", "/N", "/Q", str(sed)], check=True)

    if not out.is_file():
        print("iexpress did not produce a package", file=sys.stderr)
        return 1
    shutil.copy2(out, DIST / "setup.exe")
    print(f"wrote {DIST / 'carasoul.exe'}")
    print(f"wrote {DIST / 'setup.exe'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
