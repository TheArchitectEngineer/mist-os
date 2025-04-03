#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Regenerate the Ninja build plan and the Bazel workspace for the Fuchsia build system."""

import argparse
import filecmp
import json
import os
import shlex
import shutil
import subprocess
import sys
import typing as T
from pathlib import Path

import build_tests_json

_SCRIPT_DIR = Path(__file__).parent

# The directory that contains helper python modules for this script.
_BUILD_BAZEL_SCRIPTS = _SCRIPT_DIR / ".." / "build" / "bazel" / "scripts"
sys.path.insert(0, str(_BUILD_BAZEL_SCRIPTS))

import compute_content_hash
import remote_services_utils
import workspace_utils

_DEFAULT_HOST_TAG = "linux-x64"


def interpret_gn_path(gn_path: str, fuchsia_dir: Path, build_dir: Path) -> Path:
    """Convert a GN path value into the right Path value.

    Args:
        gn_path: The GN path string.
        fuchsia_dir: The Path value of the Fuchsia source directory.
        build_dir: The Path value of the Fuchsia build directory.
    Returns:
        A Path value.
    """
    if gn_path.startswith("//"):
        return fuchsia_dir / gn_path[2:]  # source-relative
    if gn_path.startswith("/"):
        return Path(gn_path)  # absolute path
    return build_dir / gn_path  # build-dir relative path.


def generate_bazel_content_hash_files(
    fuchsia_dir: Path,
    build_dir: Path,
    output_dir: Path,
    bazel_content_hashes: T.Any,
) -> T.Set[Path]:
    """Generate the content hash files required by the Bazel workspace.

    For each entry of the input list, write {output_dir}/{repo_name}.hash

    Args:
        fuchsia_dir: Path to Fuchsia source directory.
        build_dir: Path to the Fuchsia build directory.
        bazel_workspace: Path to the Bazel workspace.
        definitions_json: The JSON value of the `$BUILD_DIR/bazel_content_hashes.json` file
            generated at `gn gen` time (see //build/bazel:bazel_content_hashes_json).

    Returns:
        A set of input Path values to be added to the list of extra inputs for the Ninja build
        plan. This ensures that any modification to one of these files will re-trigger a call
        to //build/regenerator.
    """
    result: T.Set[Path] = set()
    for entry in bazel_content_hashes:
        # Prepare hashing state
        cipd_name = entry.get("cipd_name", None)
        cipd_names = [cipd_name] if cipd_name else []
        exclude_suffixes = entry.get("exclude_suffixes", [])
        fstate = compute_content_hash.FileState(
            cipd_names=cipd_names, exclude_suffixes=exclude_suffixes
        )

        # Hash each source path
        for source_path in entry["source_paths"]:
            input_path = interpret_gn_path(source_path, fuchsia_dir, build_dir)
            fstate.hash_source_path(input_path)

        # Write result into workspace, only update if it changed.
        repo_name = entry["repo_name"]
        output = output_dir / f"{repo_name}.hash"
        output.parent.mkdir(parents=True, exist_ok=True)
        if not output.exists() or output.read_text() != fstate.content_hash:
            output.write_text(fstate.content_hash)

        # Collect implicit inputs
        result |= fstate.get_input_file_paths()

    return result


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--fuchsia-dir",
        type=Path,
        help="Fuchsia source directory (auto-detected).",
    )
    parser.add_argument(
        "--fuchsia-build-dir",
        type=Path,
        help="Fuchsia build directory (auto-detected).",
    )
    parser.add_argument(
        "--host-tag",
        default=_DEFAULT_HOST_TAG,
        help=f"Fuchsia host tag (default {_DEFAULT_HOST_TAG})",
    )
    parser.add_argument(
        "--update-args",
        action="store_true",
        help="Update args.gn before regeneration.",
    )
    parser.add_argument(
        "--symlinks-only",
        action="store_true",
        help="Only create convenience symlinks, do not regenerate build plan.",
    )
    parser.add_argument(
        "--no-export-rust-project",
        action="store_true",
        help="Disable generation of the rust-project.json file.",
    )
    parser.add_argument(
        "--color", action="store_true", help="Force colored output."
    )
    parser.add_argument(
        "--gn-tracelog", type=Path, help="Path to GN --tracelog output file."
    )
    parser.add_argument(
        "--gn-ide",
        action="append",
        default=[],
        help="Pass value to GN --ide option.",
    )
    parser.add_argument(
        "--gn-json-ide-script",
        action="append",
        default=[],
        help="Pass value to GN --json-ide-script option.",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="count",
        default=0,
        help="Increase verbosity.",
    )
    args = parser.parse_args()

    verbose = args.verbose

    def log(msg: str) -> None:
        if verbose >= 1:
            print(msg)

    def log2(msg: str) -> None:
        if verbose >= 2:
            print(msg)

    def run_cmd(
        args: T.Sequence[str], **kwd: T.Any
    ) -> "subprocess.CompletedProcess[str]":
        if verbose >= 2:
            log("CMD: %s" % " ".join(shlex.quote(str(a)) for a in args))
        return subprocess.run(args, text=True, **kwd)

    fuchsia_dir = args.fuchsia_dir
    if fuchsia_dir:
        fuchsia_dir = fuchsia_dir.resolve()
    else:
        fuchsia_dir = workspace_utils.find_fuchsia_dir(_SCRIPT_DIR)

    build_dir = args.fuchsia_build_dir
    if build_dir:
        build_dir = build_dir.resolve()
    else:
        build_dir = workspace_utils.find_fx_build_dir(fuchsia_dir)
        if not build_dir:
            parser.error(
                "Cannot auto-detect build directory, please use --fuchsia-build-dir=DIR option!"
            )

    if not build_dir.is_dir():
        parser.error(f"Build directory missing or invalid: {build_dir}")

    if not args.symlinks_only:
        # Run `gn gen` in fuchsia_dir to regenerate the Ninja build plan.
        prebuilt_gn_subpath = f"prebuilt/third_party/gn/{args.host_tag}/gn"
        prebuilt_ninja_subpath = (
            f"prebuilt/third_party/ninja/{args.host_tag}/ninja"
        )

        build_dir_to_source_dir = os.path.relpath(fuchsia_dir, build_dir)

        gn_cmd_args = [
            prebuilt_gn_subpath,
            "args" if args.update_args else "gen",
            os.path.relpath(build_dir, fuchsia_dir),
            "--fail-on-unused-args",
            "--check=system",
            f"--ninja-executable={fuchsia_dir}/{prebuilt_ninja_subpath}",
            "--ninja-outputs-file=ninja_outputs.json",
        ]
        if not args.no_export_rust_project:
            gn_cmd_args += ["--export-rust-project"]

        if args.color:
            gn_cmd_args += ["--color"]

        if args.gn_tracelog:
            gn_cmd_args += [f"--tracelog={args.gn_tracelog}"]

        for gn_ide in args.gn_ide:
            gn_cmd_args += [f"--ide={gn_ide}"]

        for gn_json_ide_script in args.gn_json_ide_script:
            gn_cmd_args += [f"--json-ide-script={gn_json_ide_script}"]

        log("Running gn gen to rebuild Ninja manifest...")
        ret = run_cmd(gn_cmd_args, cwd=args.fuchsia_dir)
        if ret.returncode != 0:
            # Don't print anything here, assume GN already wrote something to the user.
            return ret.returncode

        log(
            "Patching Ninja build plan to invoke regenerator on build file changes."
        )

        # Patch build.ninja to ensure Ninja calls this script instead of `gn gen`
        # if any of the BUILD file changes.
        log2("- Patching build.ninja")
        build_ninja_path = build_dir / "build.ninja"
        build_ninja = build_ninja_path.read_text()
        pos = build_ninja.find("rule gn\n  command = ")
        if pos < 0:
            print(
                'ERROR: Cannot find "gn" rule in Ninja manifest!',
                file=sys.stderr,
            )
            return 1

        regenerator_command_args = [
            sys.executable,
            "-S",
            f"{build_dir_to_source_dir}/build/regenerator.py",
            # Ensure the Fuchsia checkout and build directories are relative to the
            # current directory to avoid confusing Ninja when they are moved or
            # copied to a different location. See https://fxbug.dev/401221699
            f"--fuchsia-dir={build_dir_to_source_dir}",
            "--fuchsia-build-dir=.",
            f"--host-tag={args.host_tag}",
        ]
        regenerator_command = " ".join(
            shlex.quote(str(a)) for a in regenerator_command_args
        )

        pos2 = build_ninja.find("\n", pos + 16)
        assert pos2 >= 0
        build_ninja = (
            build_ninja[:pos]
            + f"rule gn\n  command = {regenerator_command}"
            + build_ninja[pos2:]
        )
        build_ninja_path.write_text(build_ninja)

        # The list of extra inputs to add to the Ninja build plan.
        extra_ninja_build_inputs: T.Set[Path] = set()
        extra_ninja_build_inputs.add(fuchsia_dir / "build" / "regenerator")
        extra_ninja_build_inputs.add(fuchsia_dir / "build" / "regenerator.py")

        log("Generating product_bundles.json.")
        product_bundles_metadata = build_dir / "product_bundles_metadata.json"
        product_bundles = build_dir / "product_bundles.json"
        if not product_bundles.exists() or not filecmp.cmp(
            product_bundles_metadata, product_bundles
        ):
            shutil.copy(product_bundles_metadata, product_bundles)

        log("Generating tests.json.")
        extra_ninja_build_inputs |= build_tests_json.build_tests_json(build_dir)

        # Where to store regenerator outputs. This must be in a directory specific to
        # the current Ninja build directory, to support building multiple Fuchsia build
        # configurations concurrently on the same machine from the same checkout.
        #
        # For now, these are simply located in the Ninja build directory. As a consequence
        # these files can only be used as implicit inputs from GN targets. Otherwise GN
        # will complain that they belong to the root_build_dir, and that no other GN target
        # generates them.
        #
        regenerator_outputs_dir = build_dir / "regenerator_outputs"
        regenerator_outputs_dir.mkdir(parents=True, exist_ok=True)

        # Generate content hash files for the Bazel workspace.
        log("Generating Bazel content hash files")

        # Load {build_dir}/bazel_content_hashes.json generated by `gn gen`.
        # by //build/bazel:bazel_content_hashes_json
        bazel_content_hashes_json = build_dir / "bazel_content_hashes.json"
        assert (
            bazel_content_hashes_json.exists()
        ), f"Missing file after `gn gen`: {bazel_content_hashes_json}"
        with bazel_content_hashes_json.open("rb") as f:
            bazel_content_hashes = json.load(f)

        # Create corresponding content hash files under
        # $BUILD_DIR/regenerator_outputs/bazel_content_hashes/
        extra_ninja_build_inputs |= generate_bazel_content_hash_files(
            fuchsia_dir,
            build_dir,
            regenerator_outputs_dir / "bazel_content_hashes",
            bazel_content_hashes,
        )

        # Generate remote_services.bazelrc
        log("Generating remote_services.bazelrc")

        # NOTE: Since this file is generated by `gn gen`, it doesn't need
        # to be added to extra_ninja_build_inputs.
        # LINT.IfChange
        with (build_dir / "rbe_settings.json").open("rb") as f:
            rbe_settings = json.load(f)

        remote_download_outputs = rbe_settings["final"][
            "bazel_download_outputs"
        ]
        # LINT.ThenChange(//build/rbe/BUILD.gn)

        remote_services_bazelrc_file = (
            regenerator_outputs_dir / "remote_services.bazelrc"
        )
        extra_ninja_build_inputs |= (
            remote_services_utils.generate_remote_services_bazelrc(
                fuchsia_dir=fuchsia_dir,
                output_path=remote_services_bazelrc_file,
                download_outputs=remote_download_outputs,
            )
        )

        # Generate the content of the @fuchsia_build_info directory.
        log("Generating @fuchsia_build_info content")
        extra_ninja_build_inputs |= (
            workspace_utils.GnBuildArgs.generate_fuchsia_build_info(
                fuchsia_dir=fuchsia_dir,
                build_dir=build_dir,
                repository_dir=regenerator_outputs_dir / "fuchsia_build_info",
            )
        )

        # Generate the bazel launcher and Bazel workspace files.
        log("Generating Fuchsia Bazel workspace and launcher")
        extra_ninja_build_inputs |= workspace_utils.generate_fuchsia_workspace(
            fuchsia_dir=fuchsia_dir,
            build_dir=build_dir,
            log=log2,
            enable_bzlmod=False,
        )

        # Find all imported Python modules and set them as extra inputs
        # for safety. This avoids the need to manually track them, which
        # is error-prone.
        extra_ninja_build_inputs |= {
            Path(m.__file__)
            for m in sys.modules.values()
            if hasattr(m, "__file__")
        }

        sorted_extra_ninja_build_inputs = sorted(
            os.path.relpath(p, build_dir) for p in extra_ninja_build_inputs
        )

        if verbose >= 2:
            print("- Extra Ninja inputs:")
            for path in sorted_extra_ninja_build_inputs:
                print(f"  {path}")

        # Write the set of extra inputs to a file, this is used to ensure
        # that `fx bazel` can invoke regenerator if any of these changes,
        # independent of Ninja-related changes.
        log2("- Writing regenerator_inputs.txt")
        regenerator_inputs_path = (
            regenerator_outputs_dir / "regenerator_inputs.txt"
        )
        with regenerator_inputs_path.open("wt") as f:
            for input_path in sorted_extra_ninja_build_inputs:
                f.write(f"{input_path}\n")

        # Patch build.ninja.d to ensure that Ninja will re-invoke the script if its
        # own source code, or any other extra implicit input, changes.
        log2("- Patching build.ninja.d")
        build_ninja_d_path = build_dir / "build.ninja.d"
        build_ninja_d = build_ninja_d_path.read_text().rstrip()
        for extra_input in sorted_extra_ninja_build_inputs:
            build_ninja_d += f" {extra_input}"
        build_ninja_d_path.write_text(build_ninja_d)

        log2("- Updating build.ninja.stamp timestamp")
        build_ninja_stamp_path = build_dir / "build.ninja.stamp"
        build_ninja_stamp_path.touch()

    # Create convenience symlinks in fuchsia_dir, these should only be used by
    # developers manually or by IDEs, but the build should never depend on them.
    def make_relative_symlink(link_path: Path, target_path: Path) -> None:
        if link_path.is_symlink() or link_path.exists():
            link_path.unlink()
        relative_target = os.path.relpath(
            target_path.resolve(), link_path.parent.resolve()
        )
        log2(f"- Symlink {link_path} --> {relative_target}")
        link_path.symlink_to(relative_target)

    log("Creating convenience symlinks.")

    # These artifacts need to be in the source checkout.
    # NOTE: Nothing should depend on this in the build, as some developers
    # need to run multiple builds concurrently from the same source checkout
    # but using different build directories.

    # These symlinks are useful for IDEs.
    for artifact in ("compile_commands.json", "rust-project.json"):
        if (build_dir / artifact).exists():
            make_relative_symlink(fuchsia_dir / artifact, build_dir / artifact)

    # These symlink help debug Bazel issues. See //build/bazel/README.md
    make_relative_symlink(
        fuchsia_dir / "bazel-workspace",
        build_dir / "gen" / "build" / "bazel" / "workspace",
    )

    make_relative_symlink(
        fuchsia_dir / "bazel-bin",
        build_dir / "gen" / "build" / "bazel" / "workspace" / "bazel-bin",
    )

    make_relative_symlink(
        fuchsia_dir / "bazel-out",
        build_dir / "gen" / "build" / "bazel" / "workspace" / "bazel-out",
    )

    make_relative_symlink(
        fuchsia_dir / "bazel-repos",
        build_dir / "gen" / "build" / "bazel" / "output_base" / "external",
    )

    log("Done.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
