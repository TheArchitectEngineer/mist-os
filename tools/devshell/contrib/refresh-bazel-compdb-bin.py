#!/usr/bin/env fuchsia-vendored-python
# Copyright 2024 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import re
import subprocess
import sys
from typing import Any, Dict, Sequence

_CPP_EXTENSIONS = [".cc", ".c", ".cpp"]
_OPT_PATTERN = re.compile("[\W]+")

_SHOULD_LOG = False

_FUCHSIA_PACKAGE_SUFFIX = "_fuchsia_package"

_FUCHSIA_CPU_MAP = {"aarch64": "arm64", "x86_64": "x64"}

_BAZEL_CPU_ALIASES = {
    "k8": "x86_64",
    "x64": "x86_64",
    "x86_64": "x86_64",
    "aarch64": "aarch64",
    "arm64": "aarch64",
    "riscv64": "riscv64",
}


def _map_fuchsia_cpu(cpu):
    """Converts a bazel cpu to a fuchsia cpu"""
    return _FUCHSIA_CPU_MAP.get(cpu, cpu)


def _map_bazel_cpu(cpu):
    """Converts a cpu to one that bazel recognizes"""
    return _BAZEL_CPU_ALIASES.get(cpu, cpu)


# These regex patterns are a tuple of compiled regex's to lambdas that will be
# invoked with the match object if there is on. These regexes are usually used
# to transform a bazel path to one that is in GN.
_REGEX_PATH_PATTERNS = [
    # Fidl libraries defined in GN in the SDK
    (
        re.compile(
            ".*bazel-out.*\/fuchsia_sdk\/fidl\/.*\/_virtual_includes\/(?P<name>.*)_cpp"
        ),
        lambda m: "-Ifidling/gen/sdk/fidl/{fidl_lib}/{fidl_lib}/cpp".format(
            fidl_lib=m["name"]
        ),
    ),
    # Fidl libraries defined in Bazel in the SDK
    (
        re.compile(
            ".*bazel-out.*\/bin\/sdk\/fidl\/.*\/_virtual_includes\/(?P<name>.*)_cpp"
        ),
        lambda m: "-Ifidling/gen/sdk/fidl/{fidl_lib}/{fidl_lib}/cpp".format(
            fidl_lib=m["name"]
        ),
    ),
    # Fidl libraries defined in Bazel in vendor repos.
    (
        re.compile(
            ".*bazel-out.*\/bin\/vendor\/(?P<path>.*)\/fidl\/.*\/_virtual_includes\/(?P<name>.*)_cpp"
        ),
        lambda m: "-Ifidling/gen/vendor/{vendor_path}/fidl/{fidl_lib}/{fidl_lib}/cpp".format(
            vendor_path=m["path"], fidl_lib=m["name"]
        ),
    ),
    # bind libraries defined in tree under //src/devices/bind
    (
        re.compile(
            ".*bazel-out.*\/(?P<arch>[a-zA-Z0-9]+)-.*\/bin\/src\/devices\/bind\/(?P<name>.*)\/_virtual_includes.*"
        ),
        lambda m: "-I{cpu}-shared/gen/src/devices/bind/{name}/{name}/bind_cpp".format(
            cpu=_map_fuchsia_cpu(m["arch"]),
            name=m["name"],
        ),
    ),
]


class Action:
    """Represents an action that comes from aquery"""

    def __init__(self, action: Dict, target: Dict):
        self.label = target["label"]
        self.target_id = action["targetId"]
        self.action_key = action["actionKey"]
        self.arguments = action["arguments"]
        self.environment_vars = action["environmentVariables"]
        self.file = extract_file_from_args(self.arguments)

    def is_external(self) -> bool:
        return not (self.label.startswith("//") or self.label.startswith("@//"))


class CompDBFormatter:
    """A class that can convert the actions into compile_commands

    The actions that come from bazel are specific to bazel invocations and do
    not map to a command that can be passed directly to clangd. Specifically,
    the file paths are not relative to the output_root. This class will do a
    best guess on the paths to make sure they map to something that works with
    Fuchsia's out directory.
    """

    def __init__(self, build_dir: str, output_base: str, output_path: str):
        self.build_dir = build_dir
        self.output_base = output_base
        self.output_base_rel = os.path.relpath(output_base, build_dir)
        self.output_path = output_path
        self.output_path_rel = os.path.relpath(output_path, build_dir)

    def rewrite_file(self, action) -> str:
        if action.is_external():
            return os.path.join(self.output_base_rel, action.file)
        else:
            return os.path.join("../..", action.file)

    def maybe_rewrite_path(self, file_path, action) -> str:
        # Check to see if this is the file we are building. Need to take special
        # care here depending on if it is an external target or not.
        if file_path == action.file:
            return self.rewrite_file(action)

        # Bazel adds -iquote "." -iquote for files that are being compiled from
        # the internal repository. This changes those to point back to the root
        # of the fuchisa source tree.
        if file_path == ".":
            return "../../"

        # There are actions which are external that reference cc_libraries which
        # are defined as part of the main workspace, mostly @internal_sdk targets.
        # The files they reference are mainly in the //sdk, //src, //vendor and //zircon
        # directories so we need to rewrite the path and treat them as local files.
        # In the future we will likely need to do this for other cc_library targets
        # that are outside of the SDK directory and will need to find a better solution.
        if file_path.startswith(("sdk/", "src/", "vendor/", "zircon/")):
            return "../../" + file_path

        # If we are incliding a generated fidl file change it to point to the fidling
        # directory. This is needed because the fidl libraries use a _virtual_include
        # path when we run the original query which does not seem to point to a valid
        # location. Instead we can fall back to the gn generated code. This is currently
        # a best effort attempt.
        # fidl_match = _FIDL_FUCHSIA_SDK_REGEX_PATTERN.match(file_path)
        # if fidl_match:
        #     fidl_lib = fidl_match.group(1)
        #     return f"-Ifidling/gen/sdk/fidl/{fidl_lib}/{fidl_lib}/cpp"

        # Check to see if any of our regex path patterns match. These paths often
        # represent files that are generated and have _virtual_includes in the
        # path. The _virtual_includes tend to not point to files that exist when
        # working in our hybrid build system so we end up just pointing to the
        # GN paths instead.
        for pattern, replacement in _REGEX_PATH_PATTERNS:
            match_obj = pattern.match(file_path)
            if match_obj:
                return replacement(match_obj)

        # map bazel-out/ paths to that of our output_path
        if "bazel-out/" in file_path:
            return file_path.replace(
                "bazel-out/", self.output_path_rel + "/", 1
            )

        # Look for arguments to files in external/ paths. This is usually
        # the clang binary and include roots
        if "external/" in file_path:
            return file_path.replace(
                "external/",
                os.path.join(self.output_base_rel, "external") + "/",
                1,
            )

        # Just a regular argument
        return file_path

    def action_to_compile_commands(self, action: Action) -> Dict:
        return {
            "directory": self.build_dir,
            "file": self.rewrite_file(action),
            "arguments": [
                self.maybe_rewrite_path(arg, action) for arg in action.arguments
            ],
        }


def run(*command):
    try:
        return subprocess.check_output(
            command,
            text=True,
        ).strip()
    except subprocess.CalledProcessError as e:
        raise e


def list_to_dict(input: Sequence[Dict]) -> Dict:
    return {v["id"]: v for v in input}


def extract_file_from_args(args: Sequence[str]) -> str:
    """Finds the file in the action's arguments

    It would be nice to be able to get the single input file from the action but
    actions are type erased when they are returned in the query so we can't
    just grab the file that is being compiled from the arguments.
    """

    def get_ext(f):
        p = f.rfind(".")
        if p > 0:
            return f[p:]
        else:
            return ""

    files = [arg for arg in args if get_ext(arg) in _CPP_EXTENSIONS]
    assert len(files) == 1, "Should only be compiling a single file"
    return files[0]


def collect_actions(action_graph: Sequence[Dict]) -> Sequence[Action]:
    targets = list_to_dict(action_graph["targets"])
    actions = []
    for action_dict in action_graph["actions"]:
        target: Dict = targets[action_dict["targetId"]]
        action: Action = Action(action_dict, target)
        actions.append(action)
    return actions


def get_action_graph_from_labels(
    bazel_exe: str, compilation_mode: str, cpu: str, labels: Sequence[str]
) -> Sequence[Dict]:
    labels_set = "set({})".format(" ".join(labels))
    info("Getting action graph for {}".format(labels_set))
    action_graph = json.loads(
        run(
            bazel_exe,
            "aquery",
            "mnemonic('CppCompile',deps({}))".format(labels_set),
            compilation_mode,
            "--cpu={}".format(_map_bazel_cpu(cpu)),
            "--output=jsonproto",
            "--ui_event_filters=-info,-warning",
            "--noshow_loading_progress",
            "--noshow_progress",
            "--show_result=0",
        )
    )
    return collect_actions(action_graph)


def compilation_mode(args: Sequence[str]) -> str:
    # sometimes the optimization is escape quoted so we clean it up.
    opt = _OPT_PATTERN.sub("", args.optimization)
    if opt == "debug":
        return "--compilation_mode=dbg"
    elif opt in ["size", "speed", "profile", "size_lto"]:
        return "--compilation_mode=opt"
    else:
        return "--compilation_mode=fastbuild"


def canonicalize_label_from_arg(label: str) -> str:
    # fuchsia_package targets append a suffix to them which is not obvious.
    # We check the label to see if the user has appended it or not and fix
    # it for them here.
    if label.endswith(_FUCHSIA_PACKAGE_SUFFIX):
        return label
    else:
        return label + _FUCHSIA_PACKAGE_SUFFIX


def assert_arg_label_is_fuchsia_package(bazel_exe: str, label: str):
    results = collect_labels_from_scope(bazel_exe, label)
    if len(results) == 0:
        fail(
            "Provided label '{}' is not a valid fuchsia_package label. Please provide a label that points to a valid fuchsia package or use --dir instead.".format(
                label
            )
        )


def collect_labels_from_dir(args: argparse.Namespace) -> Sequence[str]:
    # Clean up the scope so it matches what bazel expects.
    dir = args.dir.removeprefix("//").removesuffix("...").removesuffix("/")
    if dir == "":
        scope = "//..."
    else:
        scope = "//{}/...".format(dir)

    return collect_labels_from_scope(args.bazel, scope)


def collect_labels_from_scope(bazel_exe: str, scope: str) -> Sequence[str]:
    try:
        return run(
            bazel_exe,
            "query",
            'kind("_build_fuchsia_package(_test)? rule", {})'.format(scope),
            "--ui_event_filters=-info,-warning",
            "--noshow_loading_progress",
            "--noshow_progress",
        ).splitlines()
    except:
        fail(
            """Unable to find any labels in {}.

        This can occur when the scope is too broad and bazel tries to query
        paths that are not compatible with bazel. For example, if you try to
        query the root directory it will pick up the prebuilt directory which
        contains files that cause the query to fail.

        Try the query again with a more limited scope.
        """.format(
                scope
            )
        )


def fail(msg: str, exit_code=1):
    print("ERROR: ", msg)
    sys.exit(exit_code)


def info(msg: str):
    if _SHOULD_LOG:
        print("INFO: ", msg)


def init_logger(args):
    global _SHOULD_LOG
    if args.verbose:
        _SHOULD_LOG = True


def is_none(obj: Any) -> bool:
    return obj == None


def main(argv: Sequence[str]):
    parser = argparse.ArgumentParser(description="Refresh bazel compdb")

    parser.add_argument("--bazel", required=True, help="The bazel binary")
    parser.add_argument(
        "--build-dir", required=True, help="The build directory"
    )
    parser.add_argument(
        "--label",
        help="The bazel label to query. This label must point to a fuchsia_package or one of its test variants.",
    )
    parser.add_argument(
        "--dir",
        help="""A directory to search for labels relative to //

        This path must be a path that we can run `fx bazel query` on. Some paths
        are not compatible with bazel queries and will fail.""",
    )
    parser.add_argument(
        "--optimization", required=True, help="The build level optimization"
    )
    parser.add_argument(
        "--target-cpu", required=True, help="The cpu we are targeting"
    )
    parser.add_argument(
        "-v",
        "--verbose",
        required=False,
        help="If we should print info logs",
        default=False,
        action="store_true",
    )
    parser.add_argument(
        "--self-test-filter",
        required=False,
        help="""If provided will run a self-test on the files that match the filter.

        The self-test will attempt to compile the file given the set of arguments
        in the compile commands. This check can be very slow because it needs to
        compile every file that matches the filter. It is directly invoking clang
        do it does not benefit from the cached results. This flag should only be
        used for debugging.

        When used in conjunction with --verbose, the command will print out the
        clang errors.

        The filter will perform a re.search on the file.
        """,
        default=None,
    )
    args = parser.parse_args(argv)
    init_logger(args)

    if is_none(args.label) and is_none(args.dir):
        fail("Either --label or --dir must be set.")

    labels = []
    if args.label:
        label = canonicalize_label_from_arg(args.label)
        info("Verifying label '{}' is valid".format(label))
        assert_arg_label_is_fuchsia_package(args.bazel, label)
        labels.append(label)

    if args.dir:
        info("Finding all labels in dir '{}'".format(args.dir))
        labels.extend(collect_labels_from_dir(args))

    actions = get_action_graph_from_labels(
        args.bazel,
        compilation_mode(args),
        args.target_cpu,
        labels,
    )

    # Output from the following bazel info command follows this format:
    #
    #   output_base: /path/to/output/base
    #   output_path: /path/to/output/path
    #
    bazel_info = run(args.bazel, "info", "output_base", "output_path").split()
    output_base = bazel_info[1]
    output_path = bazel_info[3]

    formatter = CompDBFormatter(
        args.build_dir,
        output_base,
        output_path,
    )

    new_compile_commands = []
    for action in actions:
        new_compile_commands.append(
            formatter.action_to_compile_commands(action)
        )

    compile_commands_dict = {}
    compile_commands_path = os.path.join(
        args.build_dir, "compile_commands.json"
    )
    with open(
        compile_commands_path,
        "r",
    ) as f:
        compile_commands = json.load(f)
        compile_commands.extend(new_compile_commands)
        for compile_command in compile_commands:
            compile_commands_dict[compile_command["file"]] = compile_command

    with open(
        compile_commands_path,
        "w",
    ) as f:
        json.dump(list(compile_commands_dict.values()), f, indent=2)

    if args.self_test_filter:
        commands_to_check = [
            c
            for c in compile_commands
            if re.search(args.self_test_filter, c["file"])
        ]
        info("CHECKING {} commands".format(len(commands_to_check)))
        info(
            "SKIPPING {} commands".format(
                len(compile_commands) - len(commands_to_check)
            )
        )
        num_failures = 0

        for command in commands_to_check:
            if "arguments" in command:
                clang_args = command["arguments"]
            else:
                clang_args = command["command"].split()

            try:
                subprocess.check_output(
                    clang_args,
                    text=True,
                    cwd=command["directory"],
                    stderr=None if args.verbose else subprocess.DEVNULL,
                )
            except subprocess.CalledProcessError:
                num_failures += 1

        if num_failures > 0:
            info(f"SELF TEST RESULTS: {num_failures} FAILURES")
            sys.exit(1)
        else:
            info("SELF TEST PASSED WITH NO FAILURES")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
