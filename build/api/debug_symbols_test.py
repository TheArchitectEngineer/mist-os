#!/usr/bin/env fuchsia-vendored-python
# Copyright 2025 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import json
import os
import struct
import subprocess
import sys
import tempfile
import typing as T
import unittest
from pathlib import Path

sys.path.insert(0, Path(__file__).parent)
from debug_symbols import (
    CommandPool,
    DebugSymbolExporter,
    DebugSymbolsManifestParser,
    extract_gnu_build_id,
)


def write_json(path: Path, content: T.Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("wt") as f:
        json.dump(content, f)


def generate_elf_with_build_id(build_id: bytes) -> bytes:
    """Generate a tiny ELF64 file that only contains a GNU .build-id note

    Args:
        build_id: The .build-id value as a bytes array.
    Returns:
        Content of the ELF file as a bytes array.
    """
    # This works in two passes. In the first pass, no data is generated, but write
    # positions are computed based on formatted ELF structures being appended,
    # the non-format values passed to add_struct() being ignored. This allows
    # computing all important offsets.
    #
    # On the second pass, the struct bytes are actually written to the output
    # and include the proper offset values computed in the first pass.

    def roundup8(value: int) -> int:
        return (value + 7) & -8

    class Writer(object):
        def __init__(self, enable_output: bool) -> None:
            self.offset = 0
            self.output = b""
            self.enable_output = enable_output

        def add_bytes(self, b: bytes) -> int:
            """Append a slice of bytes, then pad with zeroes for 8 alignment."""
            write_size = len(b)
            if self.enable_output:
                self.output += b
                padding = -write_size & 7
                self.output += b"\0" * padding
            self.offset = roundup8(self.offset + write_size)
            return write_size

        def add_struct(self, fmt: str, *args) -> int:
            """Append a formatted struct, then pad with zeroes for 8 alignment."""
            if not self.enable_output:
                write_size = struct.calcsize(fmt)
                self.offset = roundup8(self.offset + write_size)
                return write_size

            return self.add_bytes(struct.pack(fmt, *args))

    program_header_offset = 0
    program_header_size = 0
    program_header_count = 0

    section_header_offset = 0
    section_header_size = 0
    section_header_count = 0

    string_table_offset = 0
    note_section_offset = 0

    for enable_output in (False, True):
        writer = Writer(enable_output)

        # ELF header - https://en.wikipedia.org/wiki/Executable_and_Linkable_Format#ELF_header
        writer.add_struct(
            "<16sHHIQQQIHHHHHH",
            b"\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00",
            2,  # Type: ET_EXEC
            0x3E,  # Machine: AMD x86-64
            1,  # Version
            0,  # Entry point
            program_header_offset,  # Program header offset
            section_header_offset,  # Section header offset
            0,  # Flags
            0x40,  # ELF header size
            program_header_size,  # Program header entry size
            program_header_count,  # Program header entry count
            section_header_size,  # Section header entry size
            section_header_count,  # Section header entry count
            1,
        )  # Section header string table index

        # Program header (LOAD segment) - https://en.wikipedia.org/wiki/Executable_and_Linkable_Format#Program_header
        program_header_offset = writer.offset
        program_header_count = 1
        program_header_size = writer.add_struct(
            "<IIQQQQQQ",
            1,  # p_type: PT_LOAD
            5,  # p_flags: PF_R | PF_X
            0,  # p_offset
            0,  # p_vaddr
            0,  # p_paddr
            0x1000,  # p_filesz
            0x1000,  # p_memsz
            0x1000,
        )  # p_align
        assert program_header_size == 0x38

        # Note section data - https://docs.oracle.com/cd/E23824_01/html/819-0690/chapter6-18048.html
        # Surprisingly, there is no official spec for the content of this note. From various sources:
        # There must be a single section named .note.gnu.build-id, with a single note with vendor
        # name "GNU\x00", and type NT_GNU_BUILD_ID.
        note_section_offset = writer.offset
        note_name = b"GNU\x00"
        note_desc = build_id
        note_header = struct.pack(
            "<III", len(note_name), len(note_desc), 3
        )  # NT_GNU_BUILD_ID
        note_section_data = note_header + note_name + note_desc
        writer.add_bytes(note_section_data)

        # String table data - https://refspecs.linuxbase.org/elf/gabi4+/ch4.strtab.html
        str1_index = 1  # First symbol starts after initial 0 byte.
        string_table = b"\x00.shstrtab\x00"
        str2_index = len(string_table)
        string_table += b".note.gnu.build-id\x00"

        string_table_offset = writer.offset
        writer.add_bytes(string_table)

        # Section headers - https://en.wikipedia.org/wiki/Executable_and_Linkable_Format#Section_header
        section_header_offset = writer.offset
        section_header_count = 2
        section_header_size = writer.add_struct(
            "<IIQQQQIIQQ",
            str2_index,  # sh_name
            7,  # sh_type: SHT_NOTE
            0,  # sh_flags
            0,  # sh_addr
            note_section_offset,  # sh_offset
            len(note_section_data),  # sh_size
            0,  # sh_link
            0,  # sh_info
            8,  # sh_addralign
            0,
        )  # sh_entsize
        assert section_header_size == 0x40

        writer.add_struct(
            "<IIQQQQIIQQ",
            str1_index,  # sh_name
            7,  # sh_type: SHT_NOTE
            0,  # sh_flags
            0,  # sh_addr
            string_table_offset,  # sh_offset
            len(string_table),  # sh_size
            0,  # sh_link
            0,  # sh_info
            8,  # sh_addralign
            0,
        )  # sh_entsize

    return writer.output


# A constant .build-id value used by several tests.
TINY_ELF_BUILD_ID_VALUE = bytes.fromhex(
    "0123456789012345678901234567890123456789"
)


class ExtractGnuBuildIdTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self._root = Path(self._td.name)
        self._elf = self._root / "elf_file"

    def tearDown(self) -> None:
        self._td.cleanup()

    def _write_elf_with_build_id(self, build_id: bytes) -> None:
        self._elf.write_bytes(generate_elf_with_build_id(build_id))

    def test_1(self) -> None:
        self._write_elf_with_build_id(TINY_ELF_BUILD_ID_VALUE)
        self.assertEqual(
            extract_gnu_build_id(self._elf), TINY_ELF_BUILD_ID_VALUE.hex()
        )

        self._write_elf_with_build_id(b"\0")
        self.assertEqual(extract_gnu_build_id(self._elf), "00")

        self._write_elf_with_build_id(b"\xde\xad\xbe\xef")
        self.assertEqual(extract_gnu_build_id(self._elf), "deadbeef")


class DebugSymbolsManifestParserTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self._root = Path(self._td.name)

    def tearDown(self) -> None:
        self._td.cleanup()

    def _write_manifest(self, relpath: str, content: T.Any) -> Path:
        path = self._root / relpath
        write_json(path, content)
        return path

    def test_single_manifest(self) -> None:
        manifest = [
            {
                "label": "//src/lib/foo:lib",
                "debug": "obj/lib/foo/libfoo.so.unstripped",
                "elf_build_id_file": "obj/lib/foo/libfoo.so.elf_build_id",
                "dest_path": "lib/libfoo.so",
                "os": "fuchsia",
                "cpu": "arm64",
                "breakpad": "obj/lib/foo/libfoo.so.sym",
            }
        ]
        manifest_path = self._write_manifest("debug_symbols.json", manifest)

        parser = DebugSymbolsManifestParser(self._root)
        parser.parse_manifest_file(manifest_path)

        self.assertListEqual(parser.entries, manifest)
        self.assertSetEqual(parser.extra_input_files, set())

    def test_included_manifest(self) -> None:
        sub_manifest = [
            {
                "debug": "obj/prebuilt/packages/extracted/foo/.build-id/01/23456789.debug",
                "elf_build_id": "0123456789",
            }
        ]
        sub_relpath = "obj/prebuilt/packages/debug_symbols/foo/manifest.json"

        sub_path = self._write_manifest(sub_relpath, sub_manifest)

        manifest_path = self._write_manifest(
            "debug_symbols.json",
            [
                {
                    "label": "//prebuilt/packages/foo",
                    "manifest": sub_relpath,
                }
            ],
        )

        parser = DebugSymbolsManifestParser(self._root)
        parser.parse_manifest_file(manifest_path)

        self.assertListEqual(parser.entries, sub_manifest)
        self.assertSetEqual(parser.extra_input_files, {sub_path})

    def test_recursive_includes(self) -> None:
        top_path = self._write_manifest(
            "debug_symbols.json",
            [
                {
                    "debug": "top/a/debug_file",
                },
                {
                    "manifest": "middle/debug_symbols.json",
                },
                {
                    "debug": "top/b/debug_file",
                },
            ],
        )

        middle_path = self._write_manifest(
            "middle/debug_symbols.json",
            [
                {
                    "debug": "middle/c/debug_file",
                },
                {
                    "manifest": "bottom/debug_symbols.json",
                },
                {
                    "debug": "middle/d/debug_file",
                },
            ],
        )

        bottom_path = self._write_manifest(
            "bottom/debug_symbols.json", [{"debug": "bottom/e/debug_file"}]
        )

        parser = DebugSymbolsManifestParser(self._root)
        parser.parse_manifest_file(top_path)

        self.assertListEqual(
            parser.entries,
            [
                {
                    "debug": "top/a/debug_file",
                },
                {
                    "debug": "middle/c/debug_file",
                },
                {
                    "debug": "bottom/e/debug_file",
                },
                {
                    "debug": "middle/d/debug_file",
                },
                {
                    "debug": "top/b/debug_file",
                },
            ],
        )

        self.assertSetEqual(
            parser.extra_input_files, {middle_path, bottom_path}
        )

    def test_cycle_detection(self) -> None:
        # Write manifests such that:
        #
        #   a --> b --> c --> d
        #         ^           |
        #         |___________|
        #
        a_path = self._write_manifest(
            "a.manifest", [{"manifest": "b.manifest"}]
        )
        b_path = self._write_manifest(
            "b.manifest", [{"manifest": "c.manifest"}]
        )
        c_path = self._write_manifest(
            "c.manifest", [{"manifest": "d.manifest"}]
        )
        d_path = self._write_manifest(
            "d.manifest", [{"manifest": "b.manifest"}]
        )

        parser = DebugSymbolsManifestParser(self._root)

        with self.assertRaises(ValueError) as cm:
            parser.parse_manifest_file(a_path)

        self.assertEqual(
            str(cm.exception),
            f"""Recursive manifest includes:
  {b_path} -->
  {c_path} -->
  {d_path} -->
  {b_path}""",
        )

    def test_build_id_resolution(self) -> None:
        a_build_id_file = self._root / "a.build_id"
        a_build_id_file.write_text("build_id_for_a")

        manifest_path = self._write_manifest(
            "debug_symbols.json",
            [
                {
                    "debug": "a/libfoo.so.unstripped",
                    "elf_build_id_file": "a.build_id",
                },
                {
                    "debug": "b/libbar.so.unstripped",
                },
                {
                    "debug": "c/.build-id/bu/ild_id_for_c.debug",
                },
                {
                    "debug": "d/libzoo.so.unstripped",
                    "elf_build_id": "build_id_for_d",
                },
            ],
        )

        b_file = self._root / "b/libbar.so.unstripped"
        b_file.parent.mkdir(parents=True)
        b_file.write_bytes(generate_elf_with_build_id(TINY_ELF_BUILD_ID_VALUE))

        parser = DebugSymbolsManifestParser(self._root)
        parser.enable_build_id_resolution()
        parser.parse_manifest_file(manifest_path)

        self.maxDiff = None

        self.assertListEqual(
            parser.entries,
            [
                {
                    "debug": "a/libfoo.so.unstripped",
                    "elf_build_id": "build_id_for_a",
                    "elf_build_id_file": "a.build_id",
                },
                {
                    "debug": "b/libbar.so.unstripped",
                    "elf_build_id": TINY_ELF_BUILD_ID_VALUE.hex(),
                },
                {
                    "debug": "c/.build-id/bu/ild_id_for_c.debug",
                    "elf_build_id": "build_id_for_c",
                },
                {
                    "debug": "d/libzoo.so.unstripped",
                    "elf_build_id": "build_id_for_d",
                },
            ],
        )

        self.assertSetEqual(
            parser.extra_input_files,
            {a_build_id_file, self._root / "b/libbar.so.unstripped"},
        )


class CommandPoolTest(unittest.TestCase):
    def test_run(self) -> None:
        max_running = 0
        running_commands: set[int] = set()
        events_log: list[tuple[int, str, int]] = []

        # A CommandRunnerType value to launch a command that sleeps for a specific
        # number of seconds.
        def command_runner(
            cmd_id: int, sleep_time: float
        ) -> subprocess.Popen[str]:
            cmd_args = [
                sys.executable,
                "-c",
                "import time; time.sleep({sleep_time})",
            ]
            running_commands.add(cmd_id)
            nonlocal max_running
            if len(running_commands) > max_running:
                max_running = len(running_commands)
            events_log.append((cmd_id, "start", len(running_commands)))
            return subprocess.Popen(
                cmd_args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
            )

        def process_result(
            cmd_id: int, sleep_time: float, proc: subprocess.Popen[str]
        ) -> int:
            events_log.append((cmd_id, "stop", len(running_commands)))
            running_commands.discard(cmd_id)
            return cmd_id

        depth = 4
        count = 16
        command_pool = CommandPool(depth)
        for n in range(count):
            command_pool.add_command(0.3, command_runner)

        self.assertSetEqual(running_commands, set())
        self.assertListEqual(events_log, [])

        result_ids = []
        for result_id in command_pool.run(process_result):
            self.assertTrue(len(running_commands) <= depth)
            result_ids.append(result_id)

        # Verify that each command run exactly once.
        self.assertListEqual(sorted(result_ids), list(range(count)))

        # Verify that no more than |depth| commands ran concurrently.
        self.assertLessEqual(max_running, depth)

        # Verify there are no running commands left.
        self.assertEqual(len(running_commands), 0)

        # Verify that each command was started and stopped once.
        self.assertEqual(len(events_log), count * 2)

        started_cmds = [
            cmd_id for cmd_id, what, _ in events_log if what == "start"
        ]
        stopped_cmds = [
            cmd_id for cmd_id, what, _ in events_log if what == "stop"
        ]

        self.assertListEqual(sorted(started_cmds), list(range(count)))
        self.assertListEqual(sorted(stopped_cmds), list(range(count)))


class DebugSymbolExporterTest(unittest.TestCase):
    def setUp(self) -> None:
        self._td = tempfile.TemporaryDirectory()
        self._root = Path(self._td.name)

        # Generate several empty ELF files with a .build-id value.
        # and a debug_symbols.json manifest to list them.
        self._debug_symbol_files = {}
        self._debug_manifest = self._root / "debug_symbols.json"

        self._manifest_entries = []

        for n in range(16):
            debug_symbol_filename = f"debug_{n + 1}.so"
            build_id = bytes.fromhex("4200000000%02x" % n)
            self._manifest_entries.append(
                {
                    "debug": debug_symbol_filename,
                    "elf_build_id": build_id.hex(),
                    "label": f"//some:label_{n + 1}",
                    "os": "fuchsia",
                }
            )
            (self._root / debug_symbol_filename).write_bytes(
                generate_elf_with_build_id(build_id)
            )
            self._debug_symbol_files[build_id.hex()] = debug_symbol_filename

        # Create a fake dump_syms tool that simply prints the path of
        # the input debug symbol file.
        self._dump_syms = self._root / "dump_syms"
        self._dump_syms.write_text(
            f"""#!{sys.executable}
import argparse

parser = argparse.ArgumentParser()
parser.add_argument("-r", action="store_true")
parser.add_argument("-n")
parser.add_argument("-o")
parser.add_argument("debug_symbol_file")

args = parser.parse_args()

print(args.debug_symbol_file)
"""
        )
        self._dump_syms.chmod(0o755)

    def tearDown(self) -> None:
        self._td.cleanup()

    def test_export_debug_symbols_no_breakpad(self) -> None:
        log_lines: list[str] = []

        def log(msg: str) -> None:
            nonlocal log_lines
            log_lines.append(msg)

        err_lines: list[str] = []

        def log_error(msg: str) -> None:
            nonlocal err_lines
            err_lines.append(msg)

        exporter = DebugSymbolExporter(
            build_dir=self._root,
            dump_syms_tool=None,
            log=log,
            log_error=log_error,
        )

        exporter.parse_debug_symbols(self._manifest_entries)

        output_dir = self._root / "out"

        self.assertTrue(exporter.export_debug_symbols(output_dir))

        self.assertTrue((output_dir / ".build-id").is_dir())

        for build_id, symbol_filename in self._debug_symbol_files.items():
            debug_file = (
                output_dir
                / ".build-id"
                / build_id[0:2]
                / f"{build_id[2:]}.debug"
            )
            self.assertTrue(debug_file.exists(), msg=f"For {debug_file}")
            self.assertTrue(debug_file.is_symlink(), msg=f"For {debug_file}")
            self.assertEqual(
                debug_file.readlink(), self._root / symbol_filename
            )

        self.assertListEqual(err_lines, [])

        self.assertListEqual(
            log_lines,
            [
                f"Creating {output_dir}/build-ids.json",
                f"Creating {output_dir}/build-ids.txt",
                f"Creating 16 symlinks in {output_dir}",
            ],
        )

        build_ids_txt = output_dir / "build-ids.txt"
        self.assertTrue(build_ids_txt.is_file())
        self.assertEqual(
            build_ids_txt.read_text(),
            """\
420000000000
420000000001
420000000002
420000000003
420000000004
420000000005
420000000006
420000000007
420000000008
420000000009
42000000000a
42000000000b
42000000000c
42000000000d
42000000000e
42000000000f
""",
        )

        build_ids_json = output_dir / "build-ids.json"
        self.assertTrue(build_ids_json.is_file())
        self.assertDictEqual(
            json.loads(build_ids_json.read_text()),
            {
                "420000000000": "//some:label_1",
                "420000000001": "//some:label_2",
                "420000000002": "//some:label_3",
                "420000000003": "//some:label_4",
                "420000000004": "//some:label_5",
                "420000000005": "//some:label_6",
                "420000000006": "//some:label_7",
                "420000000007": "//some:label_8",
                "420000000008": "//some:label_9",
                "420000000009": "//some:label_10",
                "42000000000a": "//some:label_11",
                "42000000000b": "//some:label_12",
                "42000000000c": "//some:label_13",
                "42000000000d": "//some:label_14",
                "42000000000e": "//some:label_15",
                "42000000000f": "//some:label_16",
            },
        )

    def test_export_debug_symbols_with_breakpad(self) -> None:
        log_lines: list[str] = []

        def log(msg: str) -> None:
            nonlocal log_lines
            log_lines.append(msg)

        err_lines: list[str] = []

        def log_error(msg: str) -> None:
            nonlocal err_lines
            err_lines.append(msg)

        exporter = DebugSymbolExporter(
            build_dir=self._root,
            dump_syms_tool=self._dump_syms,
            log=log,
            log_error=log_error,
        )

        exporter.parse_debug_symbols(self._manifest_entries)

        output_dir = self._root / "out"
        self.assertTrue(exporter.export_debug_symbols(output_dir))

        self.assertTrue((output_dir / ".build-id").is_dir())

        for build_id, symbol_filename in self._debug_symbol_files.items():
            debug_file = (
                output_dir
                / ".build-id"
                / build_id[0:2]
                / f"{build_id[2:]}.debug"
            )
            self.assertTrue(debug_file.exists(), msg=f"For {debug_file}")
            self.assertTrue(debug_file.is_symlink(), msg=f"For {debug_file}")
            self.assertEqual(
                debug_file.readlink(), self._root / symbol_filename
            )

        self.assertListEqual(err_lines, [])

        rel_root = os.path.relpath(self._root)

        self.maxDiff = None
        self.assertListEqual(
            log_lines,
            [
                f"Creating {output_dir}/build-ids.json",
                f"Creating {output_dir}/build-ids.txt",
                f"Creating 16 symlinks in {output_dir}",
                f"Generating 16 breakpad symbols in {output_dir}",
                f"  - Creating .build-id/42/0000000000.sym FROM {rel_root}/debug_1.so",
                f"  - Creating .build-id/42/0000000001.sym FROM {rel_root}/debug_2.so",
                f"  - Creating .build-id/42/0000000002.sym FROM {rel_root}/debug_3.so",
                f"  - Creating .build-id/42/0000000003.sym FROM {rel_root}/debug_4.so",
                f"  - Creating .build-id/42/0000000004.sym FROM {rel_root}/debug_5.so",
                f"  - Creating .build-id/42/0000000005.sym FROM {rel_root}/debug_6.so",
                f"  - Creating .build-id/42/0000000006.sym FROM {rel_root}/debug_7.so",
                f"  - Creating .build-id/42/0000000007.sym FROM {rel_root}/debug_8.so",
                f"  - Creating .build-id/42/0000000008.sym FROM {rel_root}/debug_9.so",
                f"  - Creating .build-id/42/0000000009.sym FROM {rel_root}/debug_10.so",
                f"  - Creating .build-id/42/000000000a.sym FROM {rel_root}/debug_11.so",
                f"  - Creating .build-id/42/000000000b.sym FROM {rel_root}/debug_12.so",
                f"  - Creating .build-id/42/000000000c.sym FROM {rel_root}/debug_13.so",
                f"  - Creating .build-id/42/000000000d.sym FROM {rel_root}/debug_14.so",
                f"  - Creating .build-id/42/000000000e.sym FROM {rel_root}/debug_15.so",
                f"  - Creating .build-id/42/000000000f.sym FROM {rel_root}/debug_16.so",
                "Done!",
            ],
        )

        build_ids_txt = output_dir / "build-ids.txt"
        self.assertTrue(build_ids_txt.is_file())
        self.assertEqual(
            build_ids_txt.read_text(),
            """\
420000000000
420000000001
420000000002
420000000003
420000000004
420000000005
420000000006
420000000007
420000000008
420000000009
42000000000a
42000000000b
42000000000c
42000000000d
42000000000e
42000000000f
""",
        )

        build_ids_json = output_dir / "build-ids.json"
        self.assertTrue(build_ids_json.is_file())
        self.assertDictEqual(
            json.loads(build_ids_json.read_text()),
            {
                "420000000000": "//some:label_1",
                "420000000001": "//some:label_2",
                "420000000002": "//some:label_3",
                "420000000003": "//some:label_4",
                "420000000004": "//some:label_5",
                "420000000005": "//some:label_6",
                "420000000006": "//some:label_7",
                "420000000007": "//some:label_8",
                "420000000008": "//some:label_9",
                "420000000009": "//some:label_10",
                "42000000000a": "//some:label_11",
                "42000000000b": "//some:label_12",
                "42000000000c": "//some:label_13",
                "42000000000d": "//some:label_14",
                "42000000000e": "//some:label_15",
                "42000000000f": "//some:label_16",
            },
        )


if __name__ == "__main__":
    unittest.main()
