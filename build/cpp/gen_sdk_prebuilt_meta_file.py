#!/usr/bin/env fuchsia-vendored-python
# Copyright 2018 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import sys


def main():
    parser = argparse.ArgumentParser("Builds a metadata file")
    parser.add_argument("--out", help="Path to the output file", required=True)
    parser.add_argument("--name", help="Name of the library", required=True)
    parser.add_argument(
        "--format",
        help="Format of the library",
        choices=["shared", "static"],
        required=True,
    )
    parser.add_argument(
        "--root", help="Root of the library in the SDK", required=True
    )
    parser.add_argument(
        "--deps", help="Path to metadata files of dependencies", nargs="*"
    )
    parser.add_argument("--headers", help="List of public headers", nargs="*")
    parser.add_argument(
        "--include-dir", help="Path to the include directory", required=True
    )
    parser.add_argument(
        "--arch", help="Name of the target architecture", required=True
    )
    parser.add_argument(
        "--lib-link",
        help="Path to the link-time library in the SDK",
        required=True,
    )
    parser.add_argument(
        "--lib-dist",
        help="Path to the library to add to Fuchsia packages in the SDK",
        required=False,
    )
    parser.add_argument(
        "--dist-path",
        help="Path to the library in Fuchsia packages",
        required=False,
    )
    parser.add_argument(
        "--lib-debug",
        help="Path to the debug-symbols library in the SDK",
        required=False,
    )
    parser.add_argument(
        "--ifs", help="Path to an llvm .ifs file", required=False
    )
    parser.add_argument("--api-level", help="The API level", required=False)
    args = parser.parse_args()

    metadata = {
        "type": "cc_prebuilt_library",
        "name": args.name,
        "root": args.root,
        "format": args.format,
        "headers": args.headers,
        "include_dir": args.include_dir,
    }
    metadata["binaries"] = {
        args.arch: {
            "link": args.lib_link,
        },
    }

    if args.ifs:
        metadata["ifs"] = args.ifs

    if args.lib_debug:
        metadata["binaries"][args.arch]["debug"] = args.lib_debug

    if args.lib_dist:
        metadata["binaries"][args.arch]["dist"] = args.lib_dist
        metadata["binaries"][args.arch]["dist_path"] = args.dist_path

    deps = []
    for spec in args.deps:
        with open(spec, "r") as spec_file:
            data = json.load(spec_file)
        type = data["type"]
        name = data["name"]
        # TODO(https://fxbug.dev/42131085): verify that source libraries are header-only.
        if type == "cc_source_library" or type == "cc_prebuilt_library":
            deps.append(name)
        else:
            raise Exception("Unsupported dependency type: %s" % type)
    metadata["deps"] = sorted(set(deps))

    if args.api_level:
        binary = metadata["binaries"][args.arch]
        variant = {
            "constraints": {
                "arch": args.arch,
                "api_level": str(args.api_level),
            },
            "values": {},
        }
        if "dist" in binary:
            variant["values"]["dist_lib"] = binary["dist"]
        if "dist_path" in binary:
            variant["values"]["dist_lib_dest"] = binary["dist_path"]
        if "link" in binary:
            variant["values"]["link_lib"] = binary["link"]
        if "debug" in binary:
            variant["values"]["debug"] = binary["debug"]
        del metadata["binaries"]

        assert ("ifs" in metadata) == bool(
            args.ifs
        ), "--ifs option should have been added to 'ifs' key in metadata."
        if "ifs" in metadata:
            variant["values"]["ifs"] = metadata["ifs"]
            del metadata["ifs"]

        metadata["variants"] = [variant]

    with open(args.out, "w") as out_file:
        json.dump(
            metadata, out_file, indent=2, sort_keys=True, separators=(",", ": ")
        )

    return 0


if __name__ == "__main__":
    sys.exit(main())
