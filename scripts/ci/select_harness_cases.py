"""Stage a reviewed list without duplicating cases or changing their assertions."""
import argparse
from pathlib import Path
import shutil


def select(cases, manifest, destination):
    cases = cases.resolve()
    paths = [line.strip() for line in manifest.read_text().splitlines()
             if line.strip() and not line.lstrip().startswith("#")]
    if not paths or len(paths) != len(set(paths)):
        raise ValueError("case list must be nonempty and unique")
    sources = [(cases / path).resolve() for path in paths]
    if any(not source.is_relative_to(cases) or not source.is_file()
           or source.suffix not in (".yaml", ".yml") for source in sources):
        raise ValueError("case list must reference existing YAML files inside cases/")
    if len({source.name for source in sources}) != len(sources):
        raise ValueError("case list has colliding filenames")
    destination.mkdir()  # Never reuse a stale suite or hide an earlier run.
    for source in sources:
        shutil.copyfile(source, destination / source.name)
    return paths


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=Path, required=True)
    parser.add_argument("--list", type=Path, required=True)
    parser.add_argument("--destination", type=Path, required=True)
    args = parser.parse_args()
    for path in select(args.cases, args.list, args.destination):
        print(path)
