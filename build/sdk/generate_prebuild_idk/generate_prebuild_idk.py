#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Convert an IDK prebuild manifest into an IDK.

Currently works on prebuild metadata for an IDK collection but generates files
that match the corresponding IDK for a single (unmerged) collection. See
https://fxbug.dev/408003238 for discussion of some differences.
"""

import argparse
import collections
import datetime
import difflib
import json
import os
import shutil
import sys
import typing as T
from pathlib import Path

# Assume the script is in //build/sdk/generate_prebuild_idk/.
_SCRIPT_DIR = Path(__file__).parent
_FUCHSIA_ROOT_DIR = _SCRIPT_DIR.parent.parent.parent

# The directory that contains module dependencies for this script.
sys.path.insert(0, str(_SCRIPT_DIR / ".."))
# For yaml.
sys.path.insert(0, str(_FUCHSIA_ROOT_DIR / "third_party/pyyaml/src/lib"))

import generate_version_history
import yaml

AtomInfo: T.TypeAlias = T.Dict[str, T.Any]
MetaJson: T.TypeAlias = T.Dict[str, T.Any]


def get_unique_sequence(seq: T.Sequence[T.Any]) -> T.List[T.Any]:
    """Remove duplicates from an input sequence, preserving order."""
    result = []
    visited = set()
    for item in seq:
        if not item in visited:
            result.append(item)
            visited.add(item)

    return result


def collect_directory_files(src_root: str | Path) -> T.List[str]:
    """Collect list of all files from a root directory.

    Args:
        src_root: Root directory path.
    Returns:
        A list of path strings pointing to all files in the root directory.
    """
    src_root = os.path.normpath(src_root)
    result = []
    for rootpath, _dirs, dir_files in os.walk(src_root):
        for file in dir_files:
            src_path = os.path.join(rootpath, file)
            result.append(os.path.relpath(src_path, src_root))
    return result


class DebugManifest(object):
    """Models an ELF debug manifest.

    The debug manifest is a text line where each line is <dest>=<src>
    where <dest> is a destination location relative to the SDK root,
    usually starting with `.build-id/`, and <src> is a location relative
    to the Ninja build directory where the matching unstripped ELF binary
    is.
    """

    def __init__(self, content: str) -> None:
        self._map: T.Dict[str, str] = {}
        for line in content.splitlines():
            build_id_lib, sep, build_path = line.partition("=")
            assert (
                sep == "="
            ), f"Invalid debug manifest line (expected =): [{line}]"
            self._map[build_path] = build_id_lib

    def from_build_path(self, build_path: str) -> str:
        """Convert build path to unstripped library to final SDK location."""
        return self._map.get(build_path, "")

    @staticmethod
    def from_file(path: Path) -> "DebugManifest":
        """Create new instance from file path."""
        return DebugManifest(path.read_text())


class PrebuildMap(object):
    def __init__(self, prebuild_manifest: T.Sequence[AtomInfo]):
        self._labels_map: dict[str, AtomInfo] = {
            atom["atom_label"]: atom
            for atom in prebuild_manifest
            if atom["atom_type"] != "alias"
        }
        self._alias_map: dict[str, str] = {
            atom["atom_label"]: atom["atom_actual"]
            for atom in prebuild_manifest
            if atom["atom_type"] == "alias"
        }
        assert (len(self._labels_map) + len(self._alias_map)) == len(
            prebuild_manifest
        ), "Multiple atoms have the same `atom_label`."
        self._build_dir: T.Optional[Path] = None
        self._fuchsia_source_dir: T.Optional[Path] = None
        self._relative_source_prefix_from_build_dir = "../../"

    def set_build_dir(self, build_dir: Path) -> None:
        self._build_dir = build_dir.resolve()

    def set_fuchsia_source_dir(self, fuchsia_source_dir: Path) -> None:
        self._fuchsia_source_dir = fuchsia_source_dir.resolve()

    def _get_source_path(self, build_path: str) -> str:
        """Convert build directory path into source path if possible.

        Args:
            build_path: A path relative to the Ninja build directory.
        Returns:
            if the input path begins with
            self._relative_source_prefix_from_build_dir (e.g. '../../') then
            return its path relative to self._fuchsia_source_dir, otherwise this
            is a generated Ninja output, and return the empty string.
        """
        if build_path.startswith(self._relative_source_prefix_from_build_dir):
            return build_path.removeprefix(
                self._relative_source_prefix_from_build_dir
            )
        return ""

    def values(self) -> T.Sequence[AtomInfo]:
        return [*self._labels_map.values()]

    def items(self) -> T.Sequence[T.Tuple[str, AtomInfo]]:
        return [*self._labels_map.items()]

    def resolve_label(self, label: str) -> str:
        """Resolve a label through aliases."""
        return self._alias_map.get(label, label)

    def resolve_labels(self, labels: T.Sequence[str]) -> T.List[str]:
        """Resolve list of labels through aliases."""
        return [self.resolve_label(l) for l in labels]

    def resolve_unique_labels(self, labels: T.Sequence[str]) -> T.List[str]:
        """Resolve list of labels through aliases, removing duplicates."""
        return get_unique_sequence(self.resolve_labels(labels))

    def label_to_library_name(self, label: str) -> str:
        """Retrieve the library_name of a given atom label."""
        return self._labels_map[label]["prebuild_info"]["library_name"]

    def _deps_labels_to_atom_types(
        self, deps_labels: T.Sequence[str], atom_types: T.Sequence[str]
    ) -> T.List[str]:
        return sorted(
            self.label_to_library_name(d)
            for d in deps_labels
            if self._labels_map[d]["atom_type"] in atom_types
        )

    def labels_to_bind_library_names(
        self, deps_labels: T.Sequence[str]
    ) -> T.List[str]:
        """Convert a list of labels into a list of bind_library names."""
        return self._deps_labels_to_atom_types(deps_labels, ("bind_library",))

    def labels_to_fidl_library_names(
        self, deps_labels: T.Sequence[str]
    ) -> T.List[str]:
        """Convert a list of labels into a list of fidl_library names."""
        return self._deps_labels_to_atom_types(deps_labels, ("fidl_library",))

    def labels_to_cc_library_names(
        self, deps_labels: T.Sequence[str]
    ) -> T.List[str]:
        """Convert a list of labels into a list of cc_xxxx_library names."""
        return self._deps_labels_to_atom_types(
            deps_labels, ("cc_source_library", "cc_prebuilt_library")
        )

    def get_meta(
        self, info: AtomInfo
    ) -> tuple[T.Optional[MetaJson], dict[str, str]]:
        """Generate meta.json content for a given AtomInfo

        Returns a tuple containing:
        * The meta.json content or None if an unsupported type
        * A dictionary mapping additional additional atom files in the IDK to
          their sources.
        """
        value = info["atom_meta"].get("value")
        if value is not None:
            # For reference, the following types are currently handled this way:
            #   "component_manifest"
            #   "config"
            #   "data"
            #   "documentation"
            #   "ffx_tool"
            #   "host_tool"
            #   "loadable_module"
            #   "sysroot"
            return (value, {})

        generator = {
            "fidl_library": self._meta_for_fidl_library,
            "bind_library": self._meta_for_bind_library,
            "cc_prebuilt_library": self._meta_for_cc_prebuilt_library,
            "cc_source_library": self._meta_for_cc_source_library,
            "companion_host_tool": self._meta_for_companion_host_tool,
            "dart_library": self._meta_for_dart_library,
            "experimental_python_e2e_test": self._meta_for_experimental_python_e2e_test,
            "version_history": self._meta_for_version_history,
            "none": self._meta_for_noop,
            "collection": self._meta_for_collection,
            # TODO(https://fxbug.dev/338009514): Add support for Fuchsia
            # packages and add a package to `test_collection.json` along with
            # expected output to `validation_data/expected_idk/`.
            # "package":
        }.get(info["atom_type"], None)
        return generator(info) if generator else (None, {})

    def _meta_for_fidl_library(
        self, info: AtomInfo
    ) -> tuple[MetaJson, dict[str, str]]:
        prebuild = info["prebuild_info"]
        fidl_sources = [f["dest"] for f in info["atom_files"]]
        fidl_deps = self.resolve_unique_labels(prebuild.get("deps", {}))
        return {
            "name": prebuild["library_name"],
            "root": prebuild["file_base"],
            "sources": fidl_sources,
            "stable": info["is_stable"],
            "type": info["atom_type"],
            "deps": [self.label_to_library_name(d) for d in fidl_deps],
        }, {}

    def _meta_for_bind_library(
        self, info: AtomInfo
    ) -> tuple[MetaJson, dict[str, str]]:
        prebuild = info["prebuild_info"]
        bind_sources = [f["dest"] for f in info["atom_files"]]
        bind_deps = self.resolve_unique_labels(prebuild.get("deps", {}))
        return {
            "name": prebuild["library_name"],
            "root": prebuild["file_base"],
            "deps": [self.label_to_library_name(d) for d in bind_deps],
            "sources": bind_sources,
            "type": info["atom_type"],
        }, {}

    def _meta_for_cc_source_library(
        self, info: AtomInfo
    ) -> tuple[MetaJson, dict[str, str]]:
        prebuild = info["prebuild_info"]
        all_deps = self.resolve_unique_labels(prebuild.get("deps", {}))

        fidl_deps = []
        fidl_layers = collections.defaultdict(list)
        for dep_label in get_unique_sequence(prebuild.get("deps", {})):
            dep_atom = self._labels_map[self.resolve_label(dep_label)]
            if dep_atom["atom_type"] != "fidl_library":
                continue

            name = dep_atom["prebuild_info"]["library_name"]
            dep_label = dep_label.removesuffix("_sdk")
            if "_cpp" in dep_label:
                layer = "cpp" + dep_label.split("_cpp", maxsplit=1)[1]
                fidl_layers[layer].append(name)
            else:
                # There was no suffix, so this is either non-cpp binding dep or it's an hlcpp dep.
                # this covers both of those bases.
                fidl_layers["hlcpp"].append(name)
                fidl_deps.append(name)

        return {
            "name": prebuild["library_name"],
            "root": prebuild["file_base"],
            "deps": self.labels_to_cc_library_names(all_deps),
            "bind_deps": self.labels_to_bind_library_names(all_deps),
            "fidl_deps": fidl_deps,
            "fidl_binding_deps": [
                {"binding_type": layer, "deps": sorted(set(dep))}
                for layer, dep in fidl_layers.items()
            ],
            "headers": prebuild["headers"],
            "include_dir": prebuild["include_dir"],
            "sources": prebuild["sources"],
            "stable": info["is_stable"],
            "type": info["atom_type"],
        }, {}

    def _meta_for_cc_prebuilt_library(
        self, info: AtomInfo
    ) -> tuple[MetaJson, dict[str, str]]:
        prebuild = info["prebuild_info"]
        binaries = {}
        variants = []

        binary = prebuild["binaries"]
        arch = binary["arch"]
        api_level = binary["api_level"]
        dist_lib = binary.get("dist_lib")
        dist_path = binary.get("dist_path")
        link_lib = binary["link_lib"]
        debug_lib = binary.get("debug_lib", None)
        ifs_file = binary.get("ifs", None)

        # TODO(https://fxbug.dev/310006516): Remove the `if` block when the
        # `arch/` directory is removed from the IDK.
        if api_level == "PLATFORM":
            binaries[arch] = {
                "link": link_lib,
            }
            if dist_lib:
                binaries[arch]["dist"] = dist_lib
                binaries[arch]["dist_path"] = dist_path
            if debug_lib:
                binaries[arch]["debug"] = debug_lib
        else:
            variant = {
                "constraints": {
                    "api_level": api_level,
                    "arch": arch,
                },
                "values": {
                    "link_lib": link_lib,
                },
            }
            if dist_lib:
                variant["values"]["dist_lib"] = dist_lib
                variant["values"]["dist_lib_dest"] = dist_path
            if debug_lib:
                variant["values"]["debug"] = debug_lib
            if ifs_file:
                variant["values"]["ifs"] = ifs_file
            variants.append(variant)

        all_deps = self.resolve_unique_labels(prebuild.get("deps", {}))
        result = {
            "name": prebuild["library_name"],
            "root": prebuild["file_base"],
            "format": prebuild["format"],
            "headers": prebuild["headers"],
            "include_dir": prebuild["include_dir"],
            "type": info["atom_type"],
            "deps": self.labels_to_cc_library_names(all_deps),
        }
        if binaries:
            result["binaries"] = binaries
            if ifs_file:
                result["ifs"] = ifs_file
        if variants:
            result["variants"] = variants
        return (result, {})

    def _meta_for_version_history(
        self, info: AtomInfo
    ) -> tuple[MetaJson, dict[str, str]]:
        prebuild = info["prebuild_info"]
        # prebuild contains enough information to generate the final version
        # history file  by calling a Python module function.

        with (self._build_dir / prebuild["source"]).open() as f:
            version_history = json.load(f)

        with (self._build_dir / prebuild["daily_commit_hash_file"]).open() as f:
            daily_commit_hash = f.read().strip()
        with (
            self._build_dir / prebuild["daily_commit_timestamp_file"]
        ).open() as f:
            daily_commit_timestamp = datetime.datetime.fromtimestamp(
                int(f.read().strip()), datetime.UTC
            )

        generate_version_history.replace_special_abi_revisions(
            version_history, daily_commit_hash, daily_commit_timestamp
        )

        # TODO(https://fxbug.dev/383361369): Delete this once all clients have
        # been updated to use "phase" and it is removed from the real instance.
        generate_version_history.add_deprecated_status_field(version_history)

        return (version_history, {})

    def _meta_for_companion_host_tool(
        self, info: AtomInfo
    ) -> tuple[MetaJson, dict[str, str]]:
        prebuild = info["prebuild_info"]
        result = {
            "name": prebuild["name"],
            "root": "tools",
            "type": "companion_host_tool",
        }
        src_root = prebuild["src_root"]
        dest_root = prebuild["dest_root"]

        src_dir = self._get_source_path(src_root)

        binary_relpath = os.path.relpath(prebuild["binary"], src_root)
        files = [os.path.join(dest_root, binary_relpath)]
        additional_atom_files: T.Dict[str, str] = {}

        prebuilt_files = None
        if "prebuilt_files" in prebuild:
            prebuilt_files = prebuild["prebuilt_files"]
        else:
            assert src_dir
            assert self._fuchsia_source_dir
            prebuilt_files = collect_directory_files(
                self._fuchsia_source_dir / src_dir
            )

        assert prebuilt_files
        for prebuilt_file in prebuilt_files:
            source_path = os.path.join(src_root, prebuilt_file)
            dest_path = os.path.join(dest_root, prebuilt_file)
            files.append(dest_path)
            additional_atom_files[dest_path] = source_path

        # Remove duplicates if any.
        files = get_unique_sequence(files)

        # Sort all files except the first one, which must be the binary.
        result["files"] = [files[0]] + sorted(files[1:])

        return (result, additional_atom_files)

    def _meta_for_dart_library(
        self, info: AtomInfo
    ) -> tuple[MetaJson, dict[str, str]]:
        prebuild = info["prebuild_info"]

        # The list of packages that should be pulled from a Flutter SDK instead of pub.
        FLUTTER_PACKAGES = [
            "flutter",
            "flutter_driver",
            "flutter_test",
            "flutter_tools",
        ]

        third_party_deps: list[object] = []
        for spec_file in prebuild["third_party_specs"]:
            spec_file_path = os.path.normpath(self._build_dir / spec_file)
            with open(spec_file_path) as spec_f:
                manifest = yaml.safe_load(spec_f)
                name = manifest["name"]
                dep = {
                    "name": name,
                }
                if name in FLUTTER_PACKAGES:
                    dep["version"] = "flutter_sdk"
                else:
                    if "version" not in manifest:
                        raise Exception(
                            "%s does not specify a version." % spec_file
                        )
                    dep["version"] = manifest["version"]
                third_party_deps.append(dep)

        dart_deps = []
        fidl_deps = []
        for dep_label in prebuild["deps"]:
            dep_label = self.resolve_label(dep_label)
            dep_info = self._labels_map[dep_label]
            if dep_info["atom_type"] == "dart_library":
                dep_name = dep_info["prebuild"]["library_name"]
                dart_deps.append(dep_name)
            elif dep_info["atom_type"] == "fidl_library":
                dep_name = dep_info["prebuild"]["library_name"]
                fidl_deps.append(dep_name)

        result = {
            "type": "dart_library",
            "name": prebuild["library_name"],
            "root": prebuild["file_base"],
            "sources": prebuild["sources"],
            "deps": dart_deps,
            "fidl_deps": fidl_deps,
            "third_party_deps": third_party_deps,
        }
        if prebuild["null_safe"]:
            result["dart_library_null_safe"] = True
        return (result, {})

    def _meta_for_experimental_python_e2e_test(
        self, info: AtomInfo
    ) -> tuple[MetaJson, dict[str, str]]:
        prebuild = info["prebuild_info"]

        root = prebuild["file_base"]
        api_level = prebuild["api_level"]
        versioned_root = f"{root}/{api_level}"

        files: T.List[str] = []

        def get_gn_generated_path(path: str) -> str:
            if not path.startswith("GN_GENERATED("):
                return ""
            assert path[-1] == ")", f"Invalid GN generated path: {path}"
            return path.removeprefix("GN_GENERATED(")[:-1]

        test_sources_list = get_gn_generated_path(prebuild["test_sources_list"])
        assert (
            test_sources_list
        ), f'Invalid test_sources_list value: {prebuild["test_sources_list"]}'

        # Process required test sources from file.
        assert self._build_dir
        additional_atom_files: dict[str, str] = {}
        with (self._build_dir / test_sources_list).open() as f:
            test_sources_json = json.load(f)
            for entry in test_sources_json:
                dest_path = f"{versioned_root}/{entry['name']}"
                source_path = entry["path"]
                files.append(f"{dest_path}={source_path}")
                additional_atom_files[dest_path] = source_path

        return {
            "name": prebuild["name"],
            "root": root,
            "type": info["atom_type"],
            "files": [f.split("=")[0] for f in files],
        }, additional_atom_files

    def _meta_for_noop(self, info: AtomInfo) -> tuple[None, dict[str, str]]:
        return (None, {})

    def _meta_for_collection(
        self, info: AtomInfo
    ) -> tuple[MetaJson, dict[str, str]]:
        prebuild = info["prebuild_info"]
        return {
            "arch": prebuild["arch"],
            "id": info["atom_id"],
            "parts": list[dict[str, T.Any]],
            "root": prebuild["root"],
            "schema_version": prebuild["schema_version"],
        }, {}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--prebuild-manifest",
        type=Path,
        required=True,
        help="Path to the input prebuilt manifest JSON file (generated by GN gen).",
    )
    parser.add_argument(
        "--output-dir",
        type=Path,
        help=(
            "Path to the directory in which to write the IDK. "
            "A new directory will be created at this location."
            "Any existing directory will be deleted."
        ),
    )
    parser.add_argument(
        "--fuchsia-source-dir",
        type=Path,
        help="Specify the Fuchsia source directory.",
    )
    parser.add_argument(
        "--build-dir",
        type=Path,
        default=Path("out/default"),
        help="Specify the Ninja build output directory.",
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help=(
            "Verify the content of the generated files against regular ones. "
            "Does not verify whether the file should be in the IDK."
        ),
    )
    args = parser.parse_args()

    with args.prebuild_manifest.open() as f:
        prebuild_manifest = json.load(f)

    generator = IdkGenerator(
        prebuild_manifest, args.build_dir, args.fuchsia_source_dir
    )

    result = generator.GenerateMetaFileContents()
    if result != 0:
        return result

    if args.check:
        result = generator.VerifyMetaFileContentsAgainstNinjaGeneratedFiles()
        if result != 0:
            return result

    if args.output_dir:
        result = generator.WriteIdkContentsToDirectory(args.output_dir)
        if result != 0:
            return result

    return 0


class IdkGenerator(object):
    def __init__(
        self,
        prebuild_manifest: T.Sequence[AtomInfo],
        build_dir: Path,
        fuchsia_source_dir: T.Optional[Path],
    ):
        self._meta_files: T.Dict[str, MetaJson] = {}
        self._atom_files: T.Dict[str, str] = {}

        self._build_dir = build_dir

        if not fuchsia_source_dir:
            # Assume this script is under //build/sdk/generate_prebuild_idk/.
            fuchsia_source_dir = Path(__file__).parent.parent.parent.parent

        self._prebuild_map = PrebuildMap(prebuild_manifest)
        self._prebuild_map.set_fuchsia_source_dir(fuchsia_source_dir)
        self._prebuild_map.set_build_dir(build_dir)

    def GenerateMetaFileContents(self) -> int:
        """Processes the data in `self._prebuild_map`.

        Populates `self._meta_files` with the contents of meta files and
        `self._atom_files` with files to be copied.

        Must be called before other methods and may only be called one time.

        Returns:
            0 upon success and a positive integer otherwise.
        """
        assert not self._meta_files and not self._atom_files

        unhandled_labels = set()
        collection_meta_path = None
        collection_parts: list[dict[str, T.Any]] = []

        for info in self._prebuild_map.values():
            # Note: Due to the way the prebuild manifest is currently generated,
            # any atom in the "partner" category that is a dependency of the IDK
            # collection will be included in the IDK, even if it is an indirect
            # dependency, such as within a prebuilt library.
            # The IDK manifest golden build tests ensure any new IDK atoms that
            # may result from this are caught.
            if info["category"] != "partner":
                continue

            meta_json, additional_atom_files = self._prebuild_map.get_meta(info)
            assert meta_json or not additional_atom_files
            if meta_json:
                meta_path = info["atom_meta"]["dest"]
                self._meta_files[meta_path] = meta_json

                if info["atom_type"] == "collection":
                    assert (
                        not collection_meta_path
                    ), "More than one collection info provided."
                    collection_meta_path = meta_path
                else:
                    collection_parts.append(
                        {
                            "meta": meta_path,
                            "stable": info["is_stable"],
                            "type": info["atom_type"],
                        }
                    )
                    for file in info["atom_files"]:
                        dest = file["dest"]
                        source = file["source"]
                        assert (
                            dest not in self._atom_files
                        ), f"Path `{dest}` specified by multiple atoms."
                        self._atom_files[dest] = source

                if additional_atom_files:
                    for dest in additional_atom_files.keys():
                        assert (
                            dest not in self._atom_files
                        ), f"Path `{dest}` specified by multiple atoms."
                    self._atom_files.update(additional_atom_files)
            elif info["atom_type"] != "none":
                unhandled_labels.add(info["atom_label"])

        if unhandled_labels:
            print(
                "ERROR: Unhandled labels:\n%s\n"
                % "\n".join(sorted(unhandled_labels))
            )
            return 1

        collection_parts.sort(key=lambda a: (a["meta"], a["type"]))

        # Generate the IDK manifest.
        assert collection_meta_path, "Collection info must be provided."
        self._meta_files[collection_meta_path]["parts"] = collection_parts

        return 0

    def VerifyMetaFileContentsAgainstNinjaGeneratedFiles(self) -> int:
        """Verifies the meta file content generated from prebuild data against
        the files generated by the Ninja build.

        The IDK collection corresponding to the prebuild manifest must have been
        successfully built since any changes were made.

        The reference files are identified by the "atom_meta_json_file" fields
        in the prebuild metadata.

        Returns:
            0 when no differences are found and a positive integer otherwise.
        """
        assert self._meta_files

        failed = False
        for atom_label, info in self._prebuild_map.items():
            meta_path = info["atom_meta"]["dest"]
            meta_json = self._meta_files.get(meta_path)
            if meta_json is None:
                continue

            meta_json_content = json.dumps(meta_json, sort_keys=True, indent=2)

            reference_json = info["atom_meta_json_file"]
            reference_path = Path(self._build_dir / reference_json)
            if not reference_path.exists():
                print(
                    f"ERROR: Missing build file: {reference_path}",
                    file=sys.stderr,
                )
                continue

            # Some of the meta.json in the Ninja build directory are written by
            # GN directly through generated_file() and are not formatted properly,
            # so reformat them to compare their content.
            reference_content = reference_path.read_text()
            reference_content = json.dumps(
                json.loads(reference_content), sort_keys=True, indent=2
            )

            # Do the comparison, print differences.
            if meta_json_content != reference_content:
                failed = True
                print(
                    f"ERROR: meta.json differences for {atom_label}:",
                    file=sys.stderr,
                )
                print(
                    "\n".join(
                        difflib.unified_diff(
                            reference_content.splitlines(),
                            meta_json_content.splitlines(),
                            fromfile=f"from_ninja {reference_path}",
                            tofile="from_prebuild",
                            lineterm="",
                        )
                    ),
                    file=sys.stderr,
                )
                print("", file=sys.stderr)
        if failed:
            # Error message(s) were printed above.
            return 1
        else:
            print(
                "PASS: All generated metadata files have the same contents as "
                "the corresponding Ninja-built files."
            )
            return 0

    def WriteIdkContentsToDirectory(self, output_dir: Path) -> int:
        """Writes the generated contents to `output_dir`.

        Writes the metadata in `self._meta_files` to files and creates symlinks
        for each `self._atom_files`.

        Args:
            output_dir: Path to the directory in which to write the IDK.
            A new directory will be created at this location.
            Any existing directory will be deleted.
        Returns:
            0 upon success and a positive integer otherwise.
        """
        assert self._meta_files

        temp_out_dir = Path(f"{output_dir}.tmp")
        if temp_out_dir.exists():
            shutil.rmtree(temp_out_dir)

        for meta_path, meta_json in self._meta_files.items():
            dest_path = temp_out_dir / meta_path
            dest_path.parent.mkdir(parents=True, exist_ok=True)
            with dest_path.open("w") as file:
                json.dump(meta_json, file, sort_keys=True, indent=2)

        # Create symlinks for all "atom_files".
        for dest, source in self._atom_files.items():
            target_path = self._build_dir / source
            # The target directory must exist even if the file does not.
            target_path.parent.mkdir(parents=True, exist_ok=True)
            dest_path = temp_out_dir / dest
            dest_path.parent.mkdir(parents=True, exist_ok=True)

            os.symlink(target_path, dest_path)

        if output_dir.exists():
            shutil.rmtree(output_dir)
        os.rename(temp_out_dir, output_dir)
        return 0


if __name__ == "__main__":
    sys.exit(main())
