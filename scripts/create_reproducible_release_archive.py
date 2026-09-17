#!/usr/bin/env python3
"""Create a byte-reproducible Astra client release archive."""

from __future__ import annotations

import argparse
import gzip
import os
from pathlib import Path
import stat
import tarfile
import tempfile


MEMBERS = (
    ("astra", 0o755),
    ("astra-edge", 0o755),
    ("LICENSE", 0o644),
)


def parse_epoch(value: str) -> int:
    try:
        epoch = int(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("SOURCE_DATE_EPOCH must be an integer") from error
    if not 0 <= epoch <= 0xFFFFFFFF:
        raise argparse.ArgumentTypeError(
            "SOURCE_DATE_EPOCH must fit in the gzip timestamp field"
        )
    return epoch


def regular_member(source_dir: Path, name: str) -> Path:
    path = source_dir / name
    try:
        metadata = path.lstat()
    except FileNotFoundError as error:
        raise SystemExit(f"missing release archive member: {name}") from error
    if not stat.S_ISREG(metadata.st_mode):
        raise SystemExit(f"release archive member must be a regular file: {name}")
    return path


def write_archive(source_dir: Path, output: Path, epoch: int) -> None:
    members = [(name, mode, regular_member(source_dir, name)) for name, mode in MEMBERS]
    output.parent.mkdir(parents=True, exist_ok=True)
    temporary_path: Path | None = None
    try:
        with tempfile.NamedTemporaryFile(
            mode="wb", dir=output.parent, prefix=f".{output.name}.", delete=False
        ) as raw_archive:
            temporary_path = Path(raw_archive.name)
            with gzip.GzipFile(
                filename="",
                mode="wb",
                compresslevel=9,
                fileobj=raw_archive,
                mtime=epoch,
            ) as compressed:
                with tarfile.open(
                    mode="w", fileobj=compressed, format=tarfile.USTAR_FORMAT
                ) as archive:
                    for name, mode, path in members:
                        metadata = path.stat()
                        entry = tarfile.TarInfo(name=name)
                        entry.size = metadata.st_size
                        entry.mode = mode
                        entry.uid = 0
                        entry.gid = 0
                        entry.uname = "root"
                        entry.gname = "root"
                        entry.mtime = epoch
                        with path.open("rb") as source:
                            archive.addfile(entry, source)
        os.chmod(temporary_path, 0o644)
        os.replace(temporary_path, output)
        temporary_path = None
    finally:
        if temporary_path is not None:
            temporary_path.unlink(missing_ok=True)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source_dir", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("source_date_epoch", type=parse_epoch)
    arguments = parser.parse_args()

    if not arguments.source_dir.is_dir():
        raise SystemExit(
            f"release package directory does not exist: {arguments.source_dir}"
        )
    write_archive(
        arguments.source_dir.resolve(),
        arguments.output.resolve(),
        arguments.source_date_epoch,
    )


if __name__ == "__main__":
    main()
