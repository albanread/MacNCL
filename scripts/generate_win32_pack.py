#!/usr/bin/env python3
"""Generate packs/windows_api.pack from windows_api/windows_api.db.

Phase 4 of docs/WINDOWS_FFI.md. Mirrors the strategy used by the
sister NewM2 project (see E:/M2NEW/docs/win32_calls.md): one
serialized binary file that the Rust runtime loads once at
`--windows` startup, then looks up by function name.

The file format is little-endian and is documented in
src/ncl-runtime/src/win_metadata.rs. Format version is bumped any
time the layout changes; the Rust loader rejects mismatched
versions cleanly.

Type resolution
───────────────
windows_api.db stores Rust-style canonical primitives (i8/u32/…)
plus reference / struct / pointer / enum kinds for the typedefs
NCL still has to map. We don't have direction info (the column is
NULL across the board), so every parameter is treated as in.
Functions with un-mappable parameter types (callbacks, complex
structs, COM interfaces, large unions) are skipped — the
generator records the count and reports it.

Usage
─────
    python scripts/generate_win32_pack.py \\
        --db E:/windows_api/windows_api.db \\
        --output packs/windows_api.pack

Regenerate when the upstream WinMD metadata changes (the DB is
versioned via Microsoft's NuGet package; bump and re-import via
the NewM2 toolchain, then re-run this script).
"""

from __future__ import annotations

import argparse
import pathlib
import sqlite3
import struct
import sys
from typing import Optional, Tuple, List, Dict

# ─── Wire format ──────────────────────────────────────────────────────
#
# Header:
#   8 bytes magic  = b"NCLPACK1"
#   u32 format_version (current = 1)
#   u32 num_functions
#
# Per function:
#   u16 name_len
#   <name_len bytes>  function name (ASCII)
#   u16 dll_len
#   <dll_len bytes>   DLL name (ASCII)
#   u8  ret_tag
#   u8  flags          bit 0 = set_last_error; bit 1 = route_ui
#   u8  aw_byte        0 = none, 'A' (65), 'W' (87)
#   u8  num_args
#   <num_args bytes>   arg type tags
#
# All multi-byte ints are little-endian.

MAGIC = b"NCLPACK1"
FORMAT_VERSION = 1

# Type tags — must match TypeTag enum in src/ncl-runtime/src/win_ffi.rs
TAG_VOID   = 0
TAG_I8     = 1
TAG_U8     = 2
TAG_I16    = 3
TAG_U16    = 4
TAG_I32    = 5
TAG_U32    = 6
TAG_I64    = 7
TAG_U64    = 8
TAG_ISIZE  = 9
TAG_USIZE  = 10
TAG_BOOL   = 11
TAG_HANDLE = 12
TAG_PTR    = 13
TAG_WSTR   = 14
TAG_CSTR   = 15

# Primitive type_names in the DB → our tag
PRIMITIVES = {
    "void":  TAG_VOID,
    "bool":  TAG_BOOL,
    "i8":    TAG_I8,
    "u8":    TAG_U8,
    "char":  TAG_U8,    # WinMD's 'char' = uint8_t
    "i16":   TAG_I16,
    "u16":   TAG_U16,
    "i32":   TAG_I32,
    "u32":   TAG_U32,
    "i64":   TAG_I64,
    "u64":   TAG_U64,
    "isize": TAG_ISIZE,
    "usize": TAG_USIZE,
    # f32 / f64 deferred to Phase 5 (separate XMM-register dispatcher needed)
}

# Reference / struct types that are pointer-sized and ABI-compatible
# with our :handle tag (pointer-sized integer). Win32 conventionally
# treats these as opaque pointers passed in integer registers.
HANDLE_LIKE = {
    # Foundation handle types
    "HANDLE", "HWND", "HDC", "HINSTANCE", "HMODULE", "HMENU", "HICON",
    "HBITMAP", "HBRUSH", "HCURSOR", "HFONT", "HPEN", "HACCEL", "HRGN",
    "HGDIOBJ", "HHOOK", "HKEY", "HLOCAL", "HGLOBAL", "HPALETTE",
    "HDESK", "HWINSTA", "HMONITOR", "HRSRC", "HKL", "HHANDLE",
    "HCONV", "HCONVLIST", "HDDEDATA", "HSZ", "HFILE",
    "HENHMETAFILE", "HMETAFILE", "HCOLORSPACE", "HDWP",
    # Generic struct-wrapped pointer-sized things
    "LPARAM", "WPARAM", "LRESULT",
    # Errors / result codes — usually 32-bit but Win32 marshals as
    # pointer-sized in some surfaces; safer as handle.
}

# Reference-kind 32-bit ints (struct-wrapped). Pass via integer regs.
BOOL_LIKE_REFS = {
    "BOOL",       # Win32 BOOL is 32-bit int
    "BOOLEAN",    # Win32 BOOLEAN is 8-bit but ABI promotes to 32-bit in calls
}

# String pointer types
STRING_LIKE = {
    "PWSTR":  TAG_WSTR,
    "LPWSTR": TAG_WSTR,
    "PSTR":   TAG_CSTR,
    "LPSTR":  TAG_CSTR,
}


def map_type(type_name: str, kind: Optional[str]) -> Optional[int]:
    """Resolve a windows_api.db type to a tag byte, or None if not
    supported in v1 (callback, complex struct, COM iface, etc.)."""
    if type_name is None:
        return None
    # Drop the Windows.Win32.* qualification — semantically these are
    # the same type as their short-named alias.
    short = type_name.rsplit(".", 1)[-1]

    # Primitive leaves (kind='primitive')
    if kind == "primitive" and short in PRIMITIVES:
        return PRIMITIVES[short]

    # Explicit pointer kinds (type names ending in `*` in the DB)
    if short.endswith("*"):
        return TAG_PTR

    # Strings
    if short in STRING_LIKE:
        return STRING_LIKE[short]

    # Handle-like
    if short in HANDLE_LIKE:
        return TAG_HANDLE

    # BOOL family
    if short in BOOL_LIKE_REFS:
        return TAG_I32

    # Reference kind that isn't already mapped — assume handle. This
    # covers a long tail of "HXXX" types that aren't in our explicit
    # list. Some will be wrong (e.g. a 64-bit ID), but most Win32
    # references really are pointer-sized opaques.
    if kind == "reference":
        return TAG_HANDLE

    # Enums — Win32 enums default to 32-bit int.
    if kind == "enum":
        return TAG_I32

    # Pointer kind (catch-all for kind='pointer' rows)
    if kind == "pointer":
        return TAG_PTR

    # Struct passed by value (not by reference) — could be small (8
    # bytes, handle-like) or large (pass on stack). We don't yet
    # discriminate. Conservative: skip.
    if kind == "struct":
        return None

    # delegate (function pointer), interface (COM), apis-container —
    # all not yet supported.
    return None


# ─── DB query ─────────────────────────────────────────────────────────

def fetch_functions(db_path: pathlib.Path) -> Tuple[List[dict], Dict[str, int]]:
    """Walk the DB and return (records, skip_reasons). Each record is
    a dict ready to be packed."""
    conn = sqlite3.connect(str(db_path))
    # Three cursors. Sharing a cursor between an outer iteration and
    # an inner execute() resets the outer's state and silently
    # truncates results. Each level of nesting needs its own.
    outer = conn.cursor()       # iterating over functions
    param_cur = conn.cursor()   # iterating over a function's params
    type_cur = conn.cursor()    # one-off type lookups (return type, param type)
    skip = {
        "no_dll":       0,
        "variadic":     0,
        "unknown_ret":  0,
        "unknown_arg":  0,
        "non_winapi":   0,
        "no_return":    0,
    }
    records = []

    for fn_id, name, dll, callconv, ret_type_id, sle, aw_family, is_variadic in outer.execute(
        """
        SELECT function_id, function_name, dll_name, callconv, return_type_id,
               set_last_error, aw_family, is_variadic
        FROM functions
        """
    ):
        if not dll:
            skip["no_dll"] += 1
            continue
        if callconv not in ("winapi", "cdecl"):
            skip["non_winapi"] += 1
            continue
        if is_variadic:
            skip["variadic"] += 1
            continue
        if ret_type_id is None:
            skip["no_return"] += 1
            continue

        # Return type
        ret_row = type_cur.execute(
            "SELECT type_name, kind FROM types WHERE type_id = ?",
            (ret_type_id,),
        ).fetchone()
        if not ret_row:
            skip["unknown_ret"] += 1
            continue
        ret_tag = map_type(ret_row[0], ret_row[1])
        if ret_tag is None:
            skip["unknown_ret"] += 1
            continue

        # Arg types in ordinal order
        arg_tags = []
        bad = False
        for type_id, in param_cur.execute(
            """
            SELECT type_id FROM function_params
            WHERE function_id = ? ORDER BY ordinal
            """,
            (fn_id,),
        ):
            row = type_cur.execute(
                "SELECT type_name, kind FROM types WHERE type_id = ?",
                (type_id,),
            ).fetchone()
            if not row:
                bad = True
                break
            tag = map_type(row[0], row[1])
            if tag is None:
                bad = True
                break
            arg_tags.append(tag)
        if bad:
            skip["unknown_arg"] += 1
            continue

        records.append({
            "name": name,
            "dll": dll,
            "ret_tag": ret_tag,
            "arg_tags": arg_tags,
            "set_last_error": bool(sle),
            "aw_family": aw_family,   # 'A', 'W', or None
            # Default route: USER32 / GDI32 / COMCTL32 → UI; rest → any.
            # Phase 4 decision; users can override at call site.
            "route_ui": dll.upper() in {
                "USER32.DLL", "GDI32.DLL", "COMCTL32.DLL",
            },
        })

    return records, skip


# ─── Pack writer ──────────────────────────────────────────────────────

def write_pack(records: List[dict], out_path: pathlib.Path) -> None:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    with out_path.open("wb") as f:
        # Header
        f.write(MAGIC)
        f.write(struct.pack("<II", FORMAT_VERSION, len(records)))
        # Records
        for r in records:
            name = r["name"].encode("ascii", errors="strict")
            dll  = r["dll"].encode("ascii", errors="strict")
            if len(name) > 0xFFFF or len(dll) > 0xFFFF:
                raise ValueError(f"name/dll exceeds u16 length: {r['name']!r}")
            if len(r["arg_tags"]) > 0xFF:
                raise ValueError(f"arity exceeds u8: {r['name']!r}")
            f.write(struct.pack("<H", len(name)))
            f.write(name)
            f.write(struct.pack("<H", len(dll)))
            f.write(dll)
            f.write(struct.pack("<B", r["ret_tag"]))
            flags = 0
            if r["set_last_error"]: flags |= 1
            if r["route_ui"]:        flags |= 2
            f.write(struct.pack("<B", flags))
            aw_byte = 0
            if r["aw_family"] == "A": aw_byte = ord("A")
            elif r["aw_family"] == "W": aw_byte = ord("W")
            f.write(struct.pack("<B", aw_byte))
            f.write(struct.pack("<B", len(r["arg_tags"])))
            f.write(bytes(r["arg_tags"]))


# ─── CLI ──────────────────────────────────────────────────────────────

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--db", type=pathlib.Path,
        default=pathlib.Path("E:/windows_api/windows_api.db"),
        help="Path to windows_api.db (default: shared E:/windows_api/)")
    parser.add_argument("--output", type=pathlib.Path,
        default=pathlib.Path(__file__).parent.parent / "packs" / "windows_api.pack",
        help="Output pack path (default: packs/windows_api.pack in repo)")
    args = parser.parse_args()

    if not args.db.exists():
        print(f"error: DB not found: {args.db}", file=sys.stderr)
        return 1

    print(f"reading {args.db} …")
    records, skip = fetch_functions(args.db)
    print(f"kept   : {len(records):,} functions")
    print(f"skipped:")
    for reason, n in sorted(skip.items()):
        print(f"  {reason:14s} {n:,}")

    print(f"writing {args.output} …")
    write_pack(records, args.output)
    size = args.output.stat().st_size
    print(f"wrote {size:,} bytes ({size / 1024:.1f} KB)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
