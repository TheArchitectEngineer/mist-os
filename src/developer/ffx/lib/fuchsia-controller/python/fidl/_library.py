# Copyright 2023 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""The library module handles creating Python classes and types based on FIDL IR rules."""

# TODO(https://fxbug.dev/346628306): Remove this comment to ignore mypy errors.
# mypy: ignore-errors

from __future__ import annotations

import dataclasses
import inspect
import json
import os
import sys
import typing
from enum import EnumType, IntEnum, IntFlag
from types import ModuleType
from typing import (
    Any,
    Callable,
    Dict,
    ForwardRef,
    Iterable,
    List,
    Mapping,
    Optional,
    Sequence,
    Tuple,
)

from fidl_codec import add_ir_path, encode_fidl_object
from fuchsia_controller_py import Context

from ._client import EventHandlerBase, FidlClient
from ._construct import construct_response_object
from ._fidl_common import (
    FidlMeta,
    MethodInfo,
    camel_case_to_snake_case,
    internal_kind_to_type,
    normalize_identifier,
)
from ._server import ServerBase

FIDL_IR_PATH_ENV: str = "FIDL_IR_PATH"
FIDL_IR_PATH_CONFIG: str = "fidl.ir.path"
LIB_MAP: Dict[str, str] = {}
MAP_INIT = False

# Defines a mapping from import names, e.g. "fidl.foo_bar_baz" to IR representations.
#
# load_ir_from_import should be the only function that touches the IR_MAP.
IR_MAP: Dict[str, IR] = {}


class Method(dict):
    """A light wrapper around a dict that represents a FIDL method."""

    def __init__(self, parent_ir, json_dict):
        super().__init__(json_dict)
        self.parent_ir = parent_ir

    def __getitem__(self, key) -> typing.Any:
        res = super().__getitem__(key)
        if key == "identifier":
            return normalize_identifier(res)
        return res

    def has_response(self) -> bool:
        """Returns True if the method has a response."""
        return bool(self["has_response"])

    def has_request(self) -> bool:
        """Returns True if the method has a request."""
        return bool(self["has_request"])

    def has_result(self) -> bool:
        """Returns True if the method has a result.

        This is different from whether or not a method has a response, because a result is something
        that can return an error (technically it's a union with two different values).
        """
        return bool(
            self["has_error"] or (not self["strict"] and self["has_response"])
        )

    def request_payload_identifier(self) -> str | None:
        """Attempts to lookup the payload identifier if it exists.

        Returns:
            None if there is no identifier, else an identifier string.
        """
        assert "maybe_request_payload" in self
        payload = self.maybe_request_payload()
        if not payload:
            return None
        return payload.identifier()

    def response_payload_raw_identifier(self) -> str | None:
        """Attempts to lookup the response payload identifier  if it exists.

        Returns:
            None if there is no identifier, else an identifier string.
        """
        if not "maybe_response_payload" in self:
            return None
        payload = self.maybe_response_payload()
        return payload.raw_identifier() if payload is not None else None

    def maybe_response_payload(self) -> IR | None:
        if not "maybe_response_payload" in self:
            return None
        return IR(self.parent_ir, self["maybe_response_payload"])

    def maybe_request_payload(self) -> IR | None:
        if not "maybe_request_payload" in self:
            return None
        return IR(self.parent_ir, self["maybe_request_payload"])

    def ordinal(self) -> int:
        return self["ordinal"]

    def name(self) -> str:
        return normalize_member_name(self["name"])

    def raw_name(self) -> str:
        return self.name()


class IR(dict):
    """A light wrapper around a dict that contains some convenience lookup methods."""

    def __init__(self, path, json_dict):
        super().__init__(json_dict)
        self.path = path
        if "library_dependencies" in self:
            # The names of these fields are specific to how they are declared in the IR. This is so
            # they can be programmatically looked up through reflection.
            #
            # See _sorted_type_declarations for an example of looking these fields up.
            for decl in [
                "bits",
                "enum",
                "struct",
                "table",
                "union",
                "const",
                "alias",
                "protocol",
                "experimental_resource",
            ]:
                setattr(self, f"{decl}_decls", self._decl_dict(decl))

    def __getitem__(self, key):
        res = super().__getitem__(key)
        if key == "identifier":
            return normalize_identifier(res)
        if type(res) == dict:
            return IR(self.path, res)
        if type(res) == list and res and type(res[0]) == dict:
            return [IR(self.path, x) for x in res]
        return res

    def _decl_dict(self, ty: str) -> Dict[str, IR]:
        return {x["name"]: IR(self.path, x) for x in self[f"{ty}_declarations"]}

    def name(self) -> str:
        return normalize_identifier(self["name"])

    def raw_name(self) -> str:
        return super().__getitem__("name")

    def identifier(self) -> str:
        return normalize_identifier(self["identifier"])

    def raw_identifier(self) -> str:
        return super().__getitem__("identifier")

    def methods(self) -> List[Method]:
        return [Method(self, x) for x in self["methods"]]

    def declaration(self, identifier: str) -> Optional[str]:
        """Returns the declaration from the set of 'declarations,' None if not in the set.

        Args:
            identifier: The FIDL identifier, e.g. foo.bar/Baz to denote the Baz struct from library
            foo.bar. This expects a raw_identifier (which may contain underscores like _Result at
            the end of the name).

        Returns:
            The identifier's declaration type, or None if not found. The declaration type is a FIDL
            type declaration, e.g. "const," "struct," "table," etc.
        """
        return self["declarations"].get(identifier)

    def _sorted_type_declarations(self, ty: str) -> List[IR]:
        """Returns type declarations in their IR sorted order.

        Args:
            ty: The type of the declaration as a string, e.g. "const," "table," "union," etc.

        Returns:
            The declarations within this IR library, sorted by their dependency order (in order of
            least dependencies to most dependencies. An increase in dependencies means that a type
            is composited with more elements).

        This ensures that declarations, when being exported as types for the given module, are
        constructed in the correct dependency order.
        """
        return [
            getattr(self, f"{ty}_decls")[x]
            for x in self["declaration_order"]
            if self.declaration(x) == ty
        ]

    def protocol_declarations(self) -> List[IR]:
        return self._sorted_type_declarations("protocol")

    def union_declarations(self) -> List[IR]:
        return self._sorted_type_declarations("union")

    def const_declarations(self) -> List[IR]:
        return self._sorted_type_declarations("const")

    def bits_declarations(self) -> List[IR]:
        return self._sorted_type_declarations("bits")

    def enum_declarations(self) -> List[IR]:
        return self._sorted_type_declarations("enum")

    def struct_declarations(self) -> List[IR]:
        return self._sorted_type_declarations("struct")

    def table_declarations(self) -> List[IR]:
        return self._sorted_type_declarations("table")

    def alias_declarations(self) -> List[IR]:
        return self._sorted_type_declarations("alias")

    def experimental_resource_declarations(self) -> List[IR]:
        return self._sorted_type_declarations("experimental_resource")

    def resolve_kind(self, key) -> Tuple[str, str]:
        """Iteratively attempts to resolve the passed kind.

        Returns not only the kind, but the IR in which the kind was found.
        """
        kind = self.declaration(key)
        ir = self
        # If the kind is none, then it is declared in a separate library and must be imported and
        # further unwrapped.
        while kind is None:
            library = fidl_ident_to_py_import(key)
            ir = load_ir_from_import(library)
            kind = ir.declaration(key)
        return (
            kind,
            next(d for d in ir[f"{kind}_declarations"] if d["name"] == key),
        )


def find_jiri_root(starting_dir: os.PathLike) -> None | os.PathLike:
    """Returns the path to a `.jiri_root` if it can be found, else `None`."""
    current_dir = os.path.realpath(starting_dir)
    while True:
        jiri_path = os.path.join(current_dir, ".jiri_root")
        if os.path.isdir(jiri_path):
            return current_dir
        else:
            next_dir = os.path.join(current_dir, os.pardir)
            next_dir = os.path.realpath(next_dir)
            # Only happens if we're the root directory and we try to go up once
            # more.
            if current_dir == next_dir:
                return None
            current_dir = next_dir


def get_fidl_ir_map() -> Mapping[str, str]:
    """Returns a singleton mapping of library names to FIDL files."""
    global MAP_INIT
    if MAP_INIT:
        return LIB_MAP
    ctx = Context()
    # Relative path once we've found the "root" directory that should contain
    # the FIDL IR.
    ir_root_relpath = os.path.join("fidling", "gen", "ir_root")
    # TODO(b/308723467): Handle multiple paths.
    default_ir_path = ctx.config_get_string(FIDL_IR_PATH_CONFIG)
    if not default_ir_path:
        if FIDL_IR_PATH_ENV in os.environ:
            default_ir_path = os.environ[FIDL_IR_PATH_ENV]
        else:
            # We look for the root dir without using FUCHSIA_DIR env, since
            # there's a possibility the user is running multiple fuchsia
            # checkouts. We just want to check where we're running the command.
            #
            # This is the same approach as for the `.jiri_root/bin/ffx` script.
            if root_dir := find_jiri_root(os.curdir):
                with open(os.path.join(root_dir, ".fx-build-dir"), "r") as f:
                    build_dir = f.readlines()[0].strip()
                    default_ir_path = os.path.join(
                        root_dir, build_dir, ir_root_relpath
                    )
            else:
                # TODO(b/311250297): Remove last resort backstop for
                # unconfigured in-tree build config
                default_ir_path = ir_root_relpath
    if not os.path.isdir(default_ir_path):
        raise RuntimeError(
            "FIDL IR path not found via ffx config under"
            + f" '{FIDL_IR_PATH_CONFIG}', and no Fuchsia"
            + " checkout found in any parent of:"
            + f" '{os.path.realpath(os.curdir)}'."
            + f" IR also not found at default path '{default_ir_path}'."
            + f" You may need to set {FIDL_IR_PATH_ENV} in your environment,"
            + " or re-run this script from a different directory."
        )
    for _, dirs, _ in os.walk(default_ir_path):
        for d in dirs:
            LIB_MAP[d] = os.path.join(default_ir_path, d, f"{d}.fidl.json")
    MAP_INIT = True
    return LIB_MAP


def string_to_basetype(t: str) -> type:
    """Takes a base type like int32, bool, etc, and returns a Python type encapsulating it.

    Examples:
        "int32" would return the type `int`.
        "float64" would return the type `bool`.

    Returns:
        The type represented by the string.
    """
    if t.startswith("int") or t.startswith("uint"):
        return int
    elif t.startswith("float"):
        return float
    elif t == "bool":
        return bool
    else:
        raise Exception(f"Unsupported subtype: {t}")


def fidl_import_to_fidl_library(name: str) -> str:
    """Converts a fidl import, e.g. fidl.foo_bar_baz, to a fidl library: 'foo.bar.baz'"""
    assert name.startswith("fidl.")
    short_name = name[len("fidl.") :]
    short_name = short_name.replace("_", ".")
    return short_name


def fidl_import_to_library_path(name: str) -> str:
    """Returns a fidl IR path based on the fidl import name."""
    try:
        return get_fidl_ir_map()[fidl_import_to_fidl_library(name)]
    except KeyError:
        raise ImportError(
            f"Unable to import library {name}."
            + " Please ensure that the FIDL IR for this library has been created."
        )


def type_annotation(type_ir, root_ir) -> type:
    """Attempts to turn a type's IR representation into a type annotation for class constructions."""

    def wrap_optional(annotation, module=None):
        try:
            if type(annotation) == str:
                annotation = ForwardRef(annotation, module=module)
            if type_ir["nullable"]:
                return Optional[annotation]
        except KeyError:
            pass
        finally:
            return annotation

    kind = type_ir["kind_v2"]
    if kind == "identifier":
        ident = type_ir.raw_identifier()
        ty = get_type_by_identifier(ident, root_ir)
        return wrap_optional(ty)
    elif kind == "primitive":
        return string_to_basetype(type_ir["subtype"])
    elif kind == "handle":
        return wrap_optional(f"zx.{type_ir['subtype']}")
    elif kind == "string":
        return wrap_optional(str)
    elif kind == "vector" or kind == "array":
        element_type = type_ir["element_type"]
        ty = type_annotation(element_type, root_ir)
        return wrap_optional(Sequence[ty])
    elif kind == "endpoint":
        if type_ir["role"] == "client":
            protocol = type_ir["protocol"]
            module = fidl_ident_to_py_import(protocol)
            return wrap_optional(
                f"{fidl_ident_to_py_library_member(protocol)}Client",
                module=module,
            )
        elif type_ir["role"] == "server":
            protocol = type_ir["protocol"]
            module = fidl_ident_to_py_import(protocol)
            return wrap_optional(
                f"{fidl_ident_to_py_library_member(protocol)}Server",
                module=module,
            )
        else:
            raise TypeError(
                f"As yet unsupported endpoint role in library {root_ir['name']}: {type_ir['role']}"
            )
    elif kind == "internal":
        internal_kind = type_ir["subtype"]
        return internal_kind_to_type(internal_kind)
    raise TypeError(
        f"As yet unsupported type in library {root_ir['name']}: {kind}"
    )


def fidl_library_to_py_module_path(name: str) -> str:
    """Converts a fidl library, e.g. foo.bar.baz into a Python-friendly import: fidl.foo_bar_baz"""
    return "fidl." + name.replace(".", "_")


def fidl_ident_to_library(name: str) -> str:
    """Takes a fidl identifier and returns the library: foo.bar.baz/Foo would return foo.bar.baz"""
    return name.split("/")[0]


def fidl_ident_to_py_import(name: str) -> str:
    """Python import from fidl identifier: foo.bar.baz/Foo would return fidl.foo_bar_baz"""
    fidl_lib = fidl_ident_to_library(name)
    return fidl_library_to_py_module_path(fidl_lib)


def fidl_ident_to_py_library_member(name: str) -> str:
    """Returns fidl library member name from identifier: foo.bar.baz/Foo would return Foo"""
    name = normalize_identifier(name)
    return name.split("/")[1]


def docstring(decl, default: Optional[str] = None) -> Optional[str]:
    """Constructs docstring from a fidl's IR documentation declaration if it exists."""
    doc_attr = next(
        (
            attr
            for attr in decl.get("maybe_attributes", [])
            if attr["name"] == "doc"
        ),
        None,
    )
    if doc_attr is None:
        return default
    return doc_attr["arguments"][0]["value"]["value"].strip()


def bits_or_enum_root_type(ir, type_name: str) -> EnumType:
    """Constructs a Python type from either bits or enums (they are quite similar so they bottom out
    on this function)."""
    name = fidl_ident_to_py_library_member(ir.name())
    members = {
        member["name"]: int(member["value"]["value"])
        for member in ir["members"]
    }
    # Python enum types must include at least one member.
    if len(members) == 0:
        members = {"EMPTY__": 0}
    if type_name == "bits":
        ty = IntFlag(name, members)
        setattr(ty, "make_default", classmethod(lambda cls: cls(value=0)))
    else:
        # Decoding requires setting a default 0 value for enums, so
        # give the IntEnum a 0 value if it doesn't have one.
        if not 0 in members.values():
            members["EMPTY__"] = 0
        ty = IntEnum(name, members)
        setattr(ty, "make_default", classmethod(lambda cls: cls(0)))
    setattr(ty, "__fidl_kind__", type_name)
    setattr(ty, "__fidl_type__", ir.name())
    setattr(ty, "__fidl_raw_type__", ir.raw_name())
    setattr(ty, "__doc__", docstring(ir))
    setattr(ty, "__members_for_aliasing__", members)
    setattr(ty, "__strict__", bool(ir["strict"]))
    return ty


def experimental_resource_type(ir, root_ir) -> type:
    name = fidl_ident_to_py_library_member(ir.name())
    ty = type(
        name,
        (int,),
        {
            "__doc__": docstring(ir),
            "__fidl_kind__": "experimental_resource",
            "__fidl_type__": ir.name(),
            "__fidl_raw_type__": ir.raw_name(),
            "make_default": classmethod(lambda cls: 0),
        },
    )
    return ty


def bits_type(ir) -> EnumType:
    """Constructs a Python type from a bits declaration in IR."""
    return bits_or_enum_root_type(ir, "bits")


def enum_type(ir) -> EnumType:
    """Constructs a Python type from a bits declaration in IR."""
    return bits_or_enum_root_type(ir, "enum")


def union_type(ir, root_ir) -> type:
    """Constructs a Python type from a FIDL IR Union declaration."""
    __annotations__ = {}
    for variant in ir["members"]:
        variant_name = f"_{normalize_member_name(variant['name'])}"
        __annotations__[variant_name] = type_annotation(
            variant["type"], root_ir
        )

    name = fidl_ident_to_py_library_member(ir.name())
    base = type(
        name,
        (object,),
        {
            "__doc__": docstring(ir),
            "__fidl_kind__": "union",
            "__fidl_type__": ir.name(),
            "__fidl_raw_type__": ir.raw_name(),
            "__annotations__": __annotations__,
            "make_default": classmethod(lambda cls: cls(_empty=())),
            "_is_result": ir["is_result"],
        },
    )

    def __eq__(self, other):
        if not isinstance(other, type(self)):
            return False
        for internal_variant_name in base.__annotations__.keys():
            if getattr(self, internal_variant_name[1:]) != getattr(
                other, internal_variant_name[1:]
            ):
                return False
        return True

    base.__eq__ = __eq__

    def __repr__(self):
        """Returns the union repr in the format <'foo.bar.baz/FooUnion' object({value})>

        If {value} is not set, will write None."""
        variant = ""
        for variant_name in base.__annotations__.keys():
            try:
                variant = f"{variant_name}={getattr(self, variant_name)!r}"
                break
            except AttributeError:
                pass
        return f"<'{base.__fidl_type__}' object({variant})>"

    base.__repr__ = __repr__

    def __init__(self, **kwargs):
        object.__init__(self)
        if len(kwargs.keys()) == 0:
            if len(base.__annotations__) > 0:
                raise TypeError(
                    f"No variant specified: {base.__fidl_raw_type__}, {kwargs}"
                )
            return

        if len(kwargs.keys()) != 1:
            raise TypeError(
                f"Exactly one keyword argument must be specified: {base.__fidl_raw_type__}, {kwargs}"
            )

        variant_name = next(iter(kwargs.keys()))
        if variant_name == "_empty":
            return

        internal_variant_name = f"_{variant_name}"
        if internal_variant_name not in base.__annotations__:
            raise TypeError(
                f"Unexpected keyword argument for union: {base.__fidl_raw_type__}, {variant_name}"
            )
        setattr(
            self,
            internal_variant_name,
            kwargs[variant_name],
        )

    base.__init__ = __init__

    # Each Python union object stores each of its variants in an internal attribute to discourage
    # consumers of the union object from adding multiple variants. The public attribute for each
    # variant is read-only.
    for internal_variant_name in base.__annotations__.keys():

        def getter(self, _internal_variant_name=internal_variant_name):
            return getattr(self, _internal_variant_name, None)

        setattr(base, internal_variant_name[1:], property(getter))

    if ir["is_result"]:

        def unwrap(self):
            """Returns the response if result does not contain an error. Otherwise, raises an exception."""
            if (
                "_framework_err" in base.__annotations__
                and self.framework_err is not None
            ):
                raise RuntimeError(
                    f"{self.__fidl_raw_type__} framework error {self.framework_err}"
                )
            if "_err" in base.__annotations__ and self.err is not None:
                raise RuntimeError(f"{self.__fidl_raw_type__} error {self.err}")
            if (
                "_response" in base.__annotations__
                and self.response is not None
            ):
                return self.response
            raise RuntimeError(
                f"Failed to unwrap {self.__fidl_raw_type__} with no error or response."
            )

        base.unwrap = unwrap

    return base


def normalize_member_name(name) -> str:
    """Prevents use of names for struct or table members that are already keywords"""
    name = camel_case_to_snake_case(name)
    # LINT.IfChange
    if name in [
        # fmt: off
        # keep-sorted start
        "ArithmeticError",
        "AssertionError",
        "AttributeError",
        "BaseException",
        "BaseExceptionGroup",
        "BlockingIOError",
        "BrokenPipeError",
        "BufferError",
        "BytesWarning",
        "ChildProcessError",
        "ConnectionAbortedError",
        "ConnectionError",
        "ConnectionRefusedError",
        "ConnectionResetError",
        "DeprecationWarning",
        "EOFError",
        "Ellipsis",
        "EncodingWarning",
        "EnvironmentError",
        "Exception",
        "ExceptionGroup",
        "False",
        "FileExistsError",
        "FileNotFoundError",
        "FloatingPointError",
        "FutureWarning",
        "GeneratorExit",
        "IOError",
        "ImportError",
        "ImportWarning",
        "IndentationError",
        "IndexError",
        "InterruptedError",
        "IsADirectoryError",
        "KeyError",
        "KeyboardInterrupt",
        "LookupError",
        "MemoryError",
        "ModuleNotFoundError",
        "NameError",
        "None",
        "NotADirectoryError",
        "NotImplemented",
        "NotImplementedError",
        "OSError",
        "OverflowError",
        "PendingDeprecationWarning",
        "PermissionError",
        "ProcessLookupError",
        "RecursionError",
        "ReferenceError",
        "ResourceWarning",
        "RuntimeError",
        "RuntimeWarning",
        "StopAsyncIteration",
        "StopIteration",
        "SyntaxError",
        "SyntaxWarning",
        "SystemError",
        "SystemExit",
        "TabError",
        "TimeoutError",
        "True",
        "TypeError",
        "UnboundLocalError",
        "UnicodeDecodeError",
        "UnicodeEncodeError",
        "UnicodeError",
        "UnicodeTranslateError",
        "UnicodeWarning",
        "UserWarning",
        "ValueError",
        "Warning",
        "ZeroDivisionError",
        "abs",
        "aiter",
        "all",
        "and",
        "anext",
        "any",
        "as",
        "ascii",
        "assert",
        "async",
        "await",
        "bin",
        "bool",
        "break",
        "breakpoint",
        "bytearray",
        "bytes",
        "callable",
        "case",
        "chr",
        "class",
        "classmethod",
        "compile",
        "complex",
        "continue",
        "copyright",
        "credits",
        "def",
        "del",
        "delattr",
        "dict",
        "dir",
        "divmod",
        "elif",
        "else",
        "enumerate",
        "eval",
        "except",
        "exec",
        "exit",
        "filter",
        "finally",
        "float",
        "for",
        "format",
        "from",
        "frozenset",
        "getattr",
        "global",
        "globals",
        "hasattr",
        "hash",
        "help",
        "hex",
        "id",
        "if",
        "import",
        "in",
        "input",
        "int",
        "is",
        "isinstance",
        "issubclass",
        "iter",
        "lambda",
        "len",
        "license",
        "list",
        "locals",
        "map",
        "match",
        "max",
        "memoryview",
        "min",
        "next",
        "nonlocal",
        "not",
        "object",
        "oct",
        "open",
        "or",
        "ord",
        "pass",
        "pow",
        "print",
        "property",
        "quit",
        "raise",
        "range",
        "repr",
        "return",
        "reversed",
        "round",
        "self",
        "set",
        "setattr",
        "slice",
        "sorted",
        "staticmethod",
        "str",
        "sum",
        "super",
        "try",
        "tuple",
        "type",
        "vars",
        "while",
        "with",
        "yield",
        "zip",
        # keep-sorted end
        # fmt: on
    ]:
        return name + "_"
    # LINT.ThenChange(//src/developer/ffx/lib/fuchsia-controller/cpp/fidl_codec/utils.h, //src/developer/ffx/lib/fuchsia-controller/python/fidl/_library.py, //tools/fidl/fidlgen_python/codegen/ir.go, //tools/fidl/gidl/backend/fuchsia_controller/conformance.go)
    return name


def struct_and_table_subscript(self, item: str):
    if not isinstance(item, str):
        raise TypeError("Subscripted item must be a string")
    return getattr(self, item)


def struct_type(ir, root_ir) -> type:
    """Constructs a Python type from a FIDL IR struct declaration."""
    name = fidl_ident_to_py_library_member(ir.name())
    members = [
        (
            normalize_member_name(member["name"]),
            type_annotation(member["type"], root_ir),
        )
        for member in ir["members"]
    ]
    ty = dataclasses.make_dataclass(name, members)
    setattr(ty, "__fidl_kind__", "struct")
    setattr(ty, "__fidl_type__", ir.name())
    setattr(ty, "__fidl_raw_type__", ir.raw_name())
    setattr(ty, "__doc__", docstring(ir))
    setattr(ty, "__getitem__", struct_and_table_subscript)
    setattr(
        ty,
        "make_default",
        classmethod(lambda cls: cls(**{member[0]: None for member in members})),
    )
    return ty


def table_type(ir, root_ir) -> type:
    """Constructs a Python type from a FIDL IR table declaration."""
    name = fidl_ident_to_py_library_member(ir.name())
    members = []
    for member in ir["members"]:
        optional_ty = type_annotation(member["type"], root_ir)
        new_member = (
            normalize_member_name(member["name"]),
            Optional[optional_ty],
            dataclasses.field(default=None),
        )
        members.append(new_member)
    it: Iterable[Tuple[str, type, Any]] = members
    ty = dataclasses.make_dataclass(name, it)
    setattr(ty, "__fidl_kind__", "table")
    setattr(ty, "__fidl_type__", ir.name())
    setattr(ty, "__fidl_raw_type__", ir.raw_name())
    setattr(ty, "__doc__", docstring(ir))
    setattr(ty, "__getitem__", struct_and_table_subscript)
    setattr(ty, "make_default", classmethod(lambda cls: cls()))
    return ty


class FIDLConstant(object):
    def __init__(self, name, value):
        self.name = name
        self.value = value


def primitive_converter(subtype: str) -> type:
    if "int" in subtype:
        return int
    elif subtype == "bool":
        return bool
    elif "float" in subtype:
        return float
    raise TypeError(f"Unrecognized type: {subtype}")


def const_declaration(ir, root_ir) -> FIDLConstant:
    """Constructs a Python type from a FIDL IR const declaration."""
    name = fidl_ident_to_py_library_member(ir.name())
    kind = ir["type"]["kind_v2"]
    if kind == "primitive":
        converter = primitive_converter(ir["type"]["subtype"])
        return FIDLConstant(name, converter(ir["value"]["value"]))
    elif kind == "identifier":
        ident = ir["type"].identifier()
        ty = get_type_by_identifier(ident, root_ir)
        if type(ty) is str:
            return FIDLConstant(name, ty(ir["value"]["value"]))
        elif ty.__class__ == EnumType:
            return FIDLConstant(name, ty(int(ir["value"]["value"])))
        raise TypeError(
            f"As yet unsupported identifier type in lib '{root_ir['name']}': {type(ty)}"
        )
    elif kind == "string":
        return FIDLConstant(name, ir["value"]["value"])
    raise TypeError(
        f"As yet unsupported type in library '{root_ir['name']}': {kind}"
    )


def alias_declaration(ir, root_ir) -> type:
    """Constructs a Python type from a FIDL IR alias declaration."""
    name = fidl_ident_to_py_library_member(ir.name())
    ctor = ir.get("partial_type_ctor")
    if ctor:
        ctor_type = ctor["name"]
        try:
            base_type = string_to_basetype(ctor_type)
        except Exception:
            if ctor_type == "string":
                base_type = str
            elif ctor_type == "vector" or ctor_type == "array":
                # This can likely be annotated better, like constraining types.
                # There is a doc explaining some of the limitations here at go/fidl-ir-aliases
                # So for the time being this is just a generic list rather than anything specific
                # Like the struct-creating code that builds vector annotations in a more rigorous
                # way.
                base_type = list
            else:
                base_type = get_type_by_identifier(ctor_type, root_ir)
        if type(base_type) == EnumType:
            # This is a bit of a special case. Enum cannot be used as a base type when using the
            # `type` operator.
            ty = IntFlag(name, base_type.__members_for_aliasing__)
            setattr(ty, "__doc__", docstring(ir))
            setattr(ty, "__fidl_kind__", "alias")
            setattr(ty, "__fidl_type__", ir.name())
            setattr(ty, "__fidl_raw_type__", ir.raw_name())
            setattr(
                ty,
                "__members_for_aliasing__",
                base_type.__members_for_aliasing__,
            )
            return ty
        if base_type == bool:
            return bool
        base_params = {
            "__doc__": docstring(ir),
            "__fidl_kind__": "alias",
            "__fidl_type__": ir.name(),
            "__fidl_raw_type__": ir.raw_name(),
        }
        return type(name, (base_type,), base_params)


def protocol_event_handler_type(ir: IR, root_ir) -> type:
    properties = {
        "__doc__": docstring(ir),
        "__fidl_kind__": "event_handler",
        "library": root_ir.name(),
        "method_map": {},
        "construct_response_object": staticmethod(construct_response_object),
    }
    for method in ir.methods():
        # Methods without a request are event methods.
        if method.has_request():
            continue
        method_snake_case = method.name()
        properties[method_snake_case] = event_method(
            method, root_ir, get_fidl_request_server_lambda
        )
        ident = ""
        # The IR uses direction-based terminology, so an event is a method where
        # "has_request" is false and "has_response" is true (server -> client).
        # We are moving towards sequence-based terminology, where "request"
        # always means the initiating message. That's why the generated type is
        # named as *Request, but we use the "response" IR fields.
        # TODO(https://fxbug.dev/42156522): Remove this comment when the IR is updated.
        if "maybe_response_payload" in method:
            ident = method.response_payload_raw_identifier()
        properties["method_map"][method.ordinal()] = MethodInfo(
            name=method_snake_case,
            request_ident=ident,
            requires_response=False,
            empty_response=False,
            has_result=False,
            response_identifier=None,
        )
    return type(
        f"{fidl_ident_to_py_library_member(ir.name())}EventHandler",
        (EventHandlerBase,),
        properties,
    )


class ProtocolMarker(
    str,
    metaclass=FidlMeta,
):
    ...


def protocol_marker(ir: IR, root_ir) -> ProtocolMarker:
    return ProtocolMarker(normalize_identifier(ir.name().replace("/", ".")))


def protocol_server_type(ir: IR, root_ir) -> type:
    properties = {
        "__doc__": docstring(ir),
        "__fidl_kind__": "server",
        "library": root_ir.name(),
        "method_map": {},
        "construct_response_object": staticmethod(construct_response_object),
    }
    for method in ir.methods():
        method_snake_case = camel_case_to_snake_case(method.name())
        if not method.has_request():
            # This is an event. It is callable as a one-way method.
            properties[method_snake_case] = event_method(
                method, root_ir, send_event_lambda
            )
            continue
        properties[method_snake_case] = protocol_method(
            method, root_ir, get_fidl_request_server_lambda
        )
        ident = ""
        if "maybe_request_payload" in method:
            ident = method.request_payload_identifier()
        properties["method_map"][method.ordinal()] = MethodInfo(
            name=method_snake_case,
            request_ident=ident,
            requires_response=method.has_response()
            and "maybe_response_payload" in method,
            empty_response=method.has_response()
            and "maybe_response_payload" not in method,
            has_result=method.has_result(),
            response_identifier=method.response_payload_raw_identifier(),
        )
    return type(
        f"{fidl_ident_to_py_library_member(ir.name())}Server",
        (ServerBase,),
        properties,
    )


def protocol_client_type(ir: IR, root_ir) -> type:
    properties = {
        "__doc__": docstring(ir),
        "__fidl_kind__": "client",
        "construct_response_object": staticmethod(construct_response_object),
    }
    for method in ir.methods():
        if not method.has_request():
            # This is an event. This needs to be handled on its own.
            continue
        method_snake_case = method.name()
        properties[method_snake_case] = protocol_method(
            method, root_ir, get_fidl_request_client_lambda
        )
    return type(
        f"{fidl_ident_to_py_library_member(ir.name())}Client",
        (FidlClient,),
        properties,
    )


def get_fidl_method_response_payload_ident(ir: Method, root_ir) -> str:
    assert ir.has_response()
    response_ident = ""
    if ir.get("maybe_response_payload"):
        response_kind = ir.maybe_response_payload()["kind_v2"]
        if response_kind == "identifier":
            ident = ir.maybe_response_payload().raw_identifier()
            # Just ensures the module for this is going to be imported.
            get_kind_by_identifier(ident, root_ir)
            response_ident = normalize_identifier(ident)
        else:
            response_ident = response_kind
    return response_ident


def get_fidl_request_client_lambda(ir: Method, root_ir, msg) -> Callable:
    if ir.has_response():
        response_ident = get_fidl_method_response_payload_ident(ir, root_ir)
        if msg:
            return lambda self, **args: self._send_two_way_fidl_request(
                ir["ordinal"], root_ir.name(), msg(**args), response_ident
            )
        return lambda self: self._send_two_way_fidl_request(
            ir["ordinal"], root_ir.name(), msg, response_ident
        )
    if msg:
        return lambda self, **args: self._send_one_way_fidl_request(
            0, ir["ordinal"], root_ir.name(), msg(**args)
        )
    return lambda self: self._send_one_way_fidl_request(
        0, ir["ordinal"], root_ir.name(), msg
    )


def send_event_lambda(method: Method, root_ir: IR, msg) -> Callable:
    assert not method.has_request()
    if msg:
        return lambda self, *args, **kwargs: self._send_event(
            method["ordinal"], root_ir.name(), msg(*args, **kwargs)
        )

    return lambda self: self._send_event(method["ordinal"], root_ir.name(), msg)


def get_fidl_request_server_lambda(ir: Method, root_ir, msg) -> Callable:
    snake_case_name = ir.name()
    if msg:

        def server_lambda(self, request):
            raise NotImplementedError(
                f"Method {snake_case_name} not implemented"
            )

        return server_lambda
    else:

        def server_lambda(self):
            raise NotImplementedError(
                f"Method {snake_case_name} not implemented"
            )

        return server_lambda


def event_method(
    method: Method,
    root_ir: IR,
    lambda_constructor: Callable,
) -> Callable:
    assert not method.has_request()
    if "maybe_response_payload" in method:
        payload_id = method.response_payload_raw_identifier()
        (payload_kind, payload_ir) = root_ir.resolve_kind(payload_id)
    else:
        payload_id = None
        payload_kind = None
        payload_ir = None
    return create_method(
        method,
        root_ir,
        payload_id,
        payload_kind,
        payload_ir,
        lambda_constructor,
    )


def protocol_method(
    method: Method, root_ir, lambda_constructor: Callable
) -> Callable:
    assert method.has_request()
    if "maybe_request_payload" in method:
        payload_id = method.request_payload_identifier()
        (payload_kind, payload_ir) = root_ir.resolve_kind(payload_id)
    else:
        payload_id = None
        payload_kind = None
        payload_ir = None
    return create_method(
        method,
        root_ir,
        payload_id,
        payload_kind,
        payload_ir,
        lambda_constructor,
    )


def create_method(
    method: Method,
    root_ir: IR,
    payload_id: str,
    payload_kind: str,
    payload_ir: IR,
    lambda_constructor: Callable,
):
    if payload_kind == "struct":
        params = [
            inspect.Parameter(
                normalize_member_name(member["name"]),
                inspect.Parameter.KEYWORD_ONLY,
                annotation=type_annotation(member["type"], root_ir),
            )
            for member in payload_ir["members"]
        ]
        method_impl = lambda_constructor(
            method, root_ir, get_type_by_identifier(payload_id, root_ir)
        )
    elif payload_kind == "table":
        params = [
            inspect.Parameter(
                normalize_member_name(member["name"]),
                inspect.Parameter.KEYWORD_ONLY,
                default=None,
                annotation=type_annotation(member["type"], root_ir),
            )
            for member in payload_ir["members"]
        ]
        method_impl = lambda_constructor(
            method, root_ir, get_type_by_identifier(payload_id, root_ir)
        )
    elif payload_kind == "union":
        params = [
            inspect.Parameter(
                normalize_member_name(member["name"]),
                inspect.Parameter.KEYWORD_ONLY,
                annotation=type_annotation(member["type"], root_ir),
            )
            for member in payload_ir["members"]
        ]
        method_impl = lambda_constructor(
            method, root_ir, get_type_by_identifier(payload_id, root_ir)
        )
    elif payload_kind == None:
        params = []
        method_impl = lambda_constructor(method, root_ir, None)
    else:
        raise RuntimeError(
            f"Unrecognized method parameter kind: {payload_kind}"
        )

    setattr(method_impl, "__signature__", inspect.Signature(params))
    setattr(method_impl, "__doc__", docstring(method))
    setattr(method_impl, "__fidl_type__", method.name())
    setattr(method_impl, "__fidl_raw_type__", method.raw_name())
    setattr(method_impl, "__name__", method.name())
    return method_impl


def load_ir_from_import(import_name: str) -> IR:
    """Takes an import name, loads/caches the IR, and returns it."""
    lib = fidl_import_to_library_path(import_name)
    if lib not in IR_MAP:
        with open(lib, "r", encoding="UTF-8") as f:
            IR_MAP[lib] = IR(lib, json.load(f))
    return IR_MAP[lib]


def get_kind_by_identifier(ident: str, loader_ir) -> str:
    """Takes a fidl identifier, e.g. foo.bar.baz/Foo and returns its 'kind'.

    This expects a raw identifier (e.g. not one that has been normalized).

    e.g. "struct," "table," etc."""
    res = loader_ir.declaration(ident)
    if res is not None:
        return res
    return get_type_by_identifier(ident, loader_ir).__fidl_kind__


def get_type_by_identifier(ident: str, loader_ir) -> type:
    """Takes a identifier, e.g. foo.bar.baz/Foo and returns its Python type."""
    member_name = fidl_ident_to_py_library_member(ident)
    mod = load_fidl_module(fidl_ident_to_py_import(ident))
    if not hasattr(mod, member_name):
        return ForwardRef(f"{member_name}", module=mod.__name__)
    return getattr(mod, member_name)


def load_fidl_module(fullname: str) -> FIDLLibraryModule:
    if fullname not in sys.modules:
        sys.modules[fullname] = FIDLLibraryModule(fullname)
    mod = sys.modules[fullname]
    assert isinstance(
        mod, FIDLLibraryModule
    ), "load_fidl_module should only be called to load a FIDLLibraryModule"
    return mod


class FIDLLibraryModule(ModuleType):
    def __init__(self, fullname: str):
        # Shove ourselves into the import map so that composite types can be looked up as they are
        # exported.
        sys.modules[fullname] = self
        self.fullname = fullname
        ir_path = fidl_import_to_library_path(fullname)
        add_ir_path(ir_path)
        self.__ir__ = load_ir_from_import(fullname)
        self.__file__ = f"<FIDL JSON:{ir_path}>"
        self.__fullname__ = fullname
        super().__init__(
            fullname,
            docstring(self.__ir__, f"FIDL library {self.__ir__.name()}"),
        )
        self.__all__: List[str] = []

        self._export_bits()
        self._export_experimental_resources()
        self._export_enums()
        self._export_structs()
        self._export_tables()
        self._export_unions()
        self._export_consts()
        self._export_aliases()
        self._export_protocols()

    def _export_protocols(self) -> None:
        for decl in self.__ir__.protocol_declarations():
            if fidl_ident_to_py_library_member(decl.name()) not in self.__all__:
                self._export_type(
                    protocol_client_type(decl, self.__ir__),
                    include_encode=False,
                )
                self._export_type(
                    protocol_server_type(decl, self.__ir__),
                    include_encode=False,
                )
                self._export_type(
                    protocol_event_handler_type(decl, self.__ir__),
                    include_encode=False,
                )

                protocol_name = fidl_ident_to_py_library_member(decl.name())
                marker_name = f"{protocol_name}Marker"
                marker = protocol_marker(decl, self.__ir__)
                setattr(self, marker_name, marker)
                self.__all__.append(marker_name)

    def _export_structs(self) -> None:
        for decl in self.__ir__.struct_declarations():
            if fidl_ident_to_py_library_member(decl.name()) not in self.__all__:
                self._export_type(struct_type(decl, self.__ir__))

    def _export_tables(self) -> None:
        for decl in self.__ir__.table_declarations():
            if fidl_ident_to_py_library_member(decl.name()) not in self.__all__:
                self._export_type(table_type(decl, self.__ir__))

    def _export_experimental_resources(self) -> None:
        for decl in self.__ir__.experimental_resource_declarations():
            if fidl_ident_to_py_library_member(decl.name()) not in self.__all__:
                self._export_type(experimental_resource_type(decl, self.__ir__))

    def _export_bits(self) -> None:
        for decl in self.__ir__.bits_declarations():
            if fidl_ident_to_py_library_member(decl.name()) not in self.__all__:
                self._export_type(bits_type(decl))

    def _export_enums(self) -> None:
        for decl in self.__ir__.enum_declarations():
            if fidl_ident_to_py_library_member(decl.name()) not in self.__all__:
                self._export_type(enum_type(decl))

    def _export_consts(self) -> None:
        for decl in self.__ir__.const_declarations():
            if fidl_ident_to_py_library_member(decl.name()) not in self.__all__:
                self._export_fidl_const(const_declaration(decl, self.__ir__))

    def _export_aliases(self) -> None:
        for decl in self.__ir__.alias_declarations():
            name = fidl_ident_to_py_library_member(decl.name())
            if name in self.__all__:
                continue
            ty = alias_declaration(decl, self.__ir__)
            # Python doesn't allow subclassing bool, so this is a special case
            # to handle an alias for bool.
            if ty == bool:
                setattr(self, name, bool)
                self.__all__.append(name)
                continue
            self._export_type(ty)

    def _export_unions(self) -> None:
        for decl in self.__ir__.union_declarations():
            if fidl_ident_to_py_library_member(decl.name()) not in self.__all__:
                self._export_type(union_type(decl, self.__ir__))

    def _export_fidl_const(self, c: FIDLConstant) -> None:
        setattr(self, c.name, c.value)
        self.__all__.append(c.name)

    def _export_type(self, t: type, include_encode: bool = True) -> None:
        setattr(t, "__module__", self.fullname)

        if include_encode:

            def encode_func(
                # TODO(https://fxbug.dev/346628306): Use a type more specific than Any if possible.
                obj: Any,
            ) -> tuple[bytes, list[tuple[int, int, int, int, int]]]:
                library = obj.__module__
                library = library.removeprefix("fidl.")
                library = library.replace("_", ".")
                try:
                    type_name = f"{obj.__fidl_raw_type__}"
                except AttributeError:
                    type_name = f"{library}/{type(obj).__name__}"
                finally:
                    return encode_fidl_object(obj, library, type_name)

            setattr(t, "encode", encode_func)
        setattr(self, t.__name__, t)
        self.__all__.append(t.__name__)
