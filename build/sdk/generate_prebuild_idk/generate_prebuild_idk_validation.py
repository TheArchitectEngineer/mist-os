#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Unit-test for generate_prebuild_idk.py"""

import argparse
import difflib
import filecmp
import os
import subprocess
import sys
import tempfile
import typing as T
from pathlib import Path

_SCRIPT_DIR = Path(__file__).parent
sys.path.insert(0, str(_SCRIPT_DIR))

# Assume the script is in //build/sdk/generate_prebuild_idk/.
_FUCHSIA_ROOT_DIR = _SCRIPT_DIR.parent.parent.parent


# TODO(https://fxbug.dev/338009514): Share with
# //build/bazel/fuchsia_idk/generate_repository_validation.py.
def _get_files_from(top_dir: Path) -> set[str]:
    """Walk the top_dir directory, and return a set of relative paths to all its files."""
    return {
        os.path.relpath(os.path.join(dirpath, filename), top_dir)
        for dirpath, dirnames, filenames in os.walk(top_dir)
        for filename in filenames
    }


# TODO(https://fxbug.dev/338009514): Share with
# //build/bazel/fuchsia_idk/generate_repository_validation.py.
def compare_directories(
    left_dir: Path, right_dir: Path
) -> T.Tuple[T.Sequence[str], T.Sequence[str], T.Sequence[str]]:
    """Compare the content of two directories.

    Args:
        left: Path to first directory.
        right: Path to second directory.
    Returns:
        A 3-tuple whose item correspond to the following lists or
        relative path strings:

        - different_files: files that are present in both directories,
          but whose content differs.
        - left_only_file: files that only appear in the `left` directory.
        - right_only_files: files that only appear in the `right` directory.
    """
    left_files = _get_files_from(left_dir)
    right_files = _get_files_from(right_dir)

    left_only_files = left_files - right_files
    right_only_files = right_files - left_files

    # Find files that are different.
    different_files = set()
    common_files = left_files & right_files
    for file in common_files:
        left = left_dir / file
        right = right_dir / file
        if left.is_symlink() != right.is_symlink():
            different_files.add(file)
            continue

        if left.is_symlink():
            if (
                not right.is_symlink()
                or (right.parent / right.readlink()).resolve()
                != (left.parent / left.readlink()).resolve()
            ):
                different_files.add(file)
            continue

        if not filecmp.cmp(left, right, shallow=False):
            different_files.add(file)
            continue

    return (
        sorted(different_files),
        sorted(left_only_files),
        sorted(right_only_files),
    )


def main() -> int:
    _generate_idk_script = _SCRIPT_DIR / "generate_prebuild_idk.py"
    _test_data_dir = _SCRIPT_DIR / "validation_data"
    _input_fuchsia_source_dir = _test_data_dir / "input_fuchsia_dir"
    _input_build_dir = _input_fuchsia_source_dir / "out/notdefault"
    _prebuild_manifest = _test_data_dir / "test_collection.json"
    _expected_idk_dir = _test_data_dir / "expected_idk"

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--hermetic-inputs-file",
        type=Path,
        help="Output file path to write the list of implicit inputs for this script.",
    )
    parser.add_argument(
        "--quiet", action="store_true", help="Do not print anything on success."
    )
    parser.add_argument(
        "--stamp-file", type=Path, help="Output stamp file path."
    )

    args = parser.parse_args()

    if args.hermetic_inputs_file:
        # Generate a file containing the list of hermetic inputs. Results must
        # be relative to the current working directory.

        implicit_inputs = []
        for root, subdirs, subfiles in os.walk(_test_data_dir):
            for subfile in subfiles:
                implicit_inputs.append(
                    os.path.relpath(os.path.join(root, subfile))
                )

        args.hermetic_inputs_file.parent.mkdir(parents=True, exist_ok=True)
        args.hermetic_inputs_file.write_text("\n".join(sorted(implicit_inputs)))
        return 0

    temp_dir = tempfile.TemporaryDirectory(
        prefix="generate_idk_repository-integration-"
    )
    output_idk_dir = Path(temp_dir.name)

    generate_prebuild_command_args = [
        sys.executable,
        "-S",
        str(_generate_idk_script),
        f"--fuchsia-source-dir={_input_fuchsia_source_dir}",
        f"--build-dir={_input_build_dir}",
        f"--prebuild-manifest={_prebuild_manifest}",
        f"--output-dir={output_idk_dir}",
    ]

    ret = subprocess.run(
        generate_prebuild_command_args,
    )
    ret.check_returncode()

    diff_files, left_only, right_only = compare_directories(
        _expected_idk_dir, output_idk_dir
    )

    errors: list[str] = []
    if left_only:
        errors.append(
            "Missing files from the output IDK:\n  %s\n"
            % ("\n  ".join(sorted(left_only)))
        )
    if right_only:
        # For now, ignore symlinks that are only in the output directory.
        all_are_symlinks = True
        for f in right_only:
            if not (output_idk_dir / f).is_symlink():
                all_are_symlinks = False

        if all_are_symlinks:
            if not args.quiet:
                print("Ignoring symlinks found only in the output directory.")
        else:
            errors.append(
                "Unexpected files in the output IDK:\n  %s\n"
                % ("\n  ".join(sorted(right_only)))
            )
    if diff_files:
        errors.append("Differences between golden and output IDK:")
        for file_name in diff_files:
            left_file = _expected_idk_dir / file_name
            right_file = output_idk_dir / file_name
            if not left_file.exists():
                errors.append(f"< INVALID SYMLINK {left_file}")
            if not right_file.exists():
                errors.append(f"> INVALID SYMLINK {right_file}")
            if left_file.exists() and right_file.exists():
                diff_lines = difflib.unified_diff(
                    left_file.read_text().splitlines(),
                    right_file.read_text().splitlines(),
                    f"_expected_idk_dir/{file_name}",
                    f"output_idk_dir/{file_name}",
                    lineterm="",
                )
                for line in diff_lines:
                    errors.append("   " + line)

    if errors:
        print("ERRORS:\n" + "\n".join(errors), file=sys.stderr)
        return 1

    if args.stamp_file:
        args.stamp_file.write_text("")

    if not args.quiet:
        print("PASS")

    return 0


if __name__ == "__main__":
    sys.exit(main())
