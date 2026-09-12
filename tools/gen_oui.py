#!/usr/bin/env python3
"""Generate src/ui/oui.bin from the IEEE MA-L (OUI) registry.

Data: IEEE Registration Authority, https://standards-oui.ieee.org/oui/oui.csv
(the Wireshark `manuf` list is the fallback if the IEEE site refuses the
request; the Rust table already committed at OLD_RUST is the last-resort
fallback if neither network source is reachable). Run once; the output blob
is committed and loaded by `src/ui/oui_table.rs` via `include_bytes!`.

    python tools/gen_oui.py

Blob format (little-endian), read by `oui_table.rs`:
    b"OUI1"                              4-byte magic
    u32 n_prefixes, u32 n_vendors        header counts
    n_prefixes x u32                     sorted 24-bit OUI prefixes
    n_prefixes x u16                     vendor index, parallel to the above
    u32 total_len                        byte length of the string table
    total_len bytes                      UTF-8 vendor names, concatenated
    n_vendors x u32                      start offset of each name in the
                                          string table (end = next start, or
                                          total_len for the last vendor)
"""
import csv
import io
import os
import re
import struct
import sys
import urllib.request
from datetime import date

IEEE = "https://standards-oui.ieee.org/oui/oui.csv"
WIRESHARK = "https://raw.githubusercontent.com/wireshark/wireshark/master/manuf"
OLD_RUST = os.path.join(os.path.dirname(__file__), "..", "src", "ui", "oui_table.rs")
OUT = os.path.join(os.path.dirname(__file__), "..", "src", "ui", "oui.bin")
MAX_LEN = 24
UA = {"User-Agent": "Mozilla/5.0 (pipboy oui table generator)"}

# Corporate noise at the end of a name. "Electronics" is deliberately absent:
# "Samsung Electronics Co.,Ltd" should keep it.
SUFFIXES = {
    "inc", "incorporated", "llc", "llp", "ltd", "limited", "co", "corp",
    "corporation", "company", "gmbh", "mbh", "ag", "sa", "sas", "sarl", "srl",
    "bv", "nv", "plc", "pty", "oy", "oyj", "ab", "as", "a/s", "kg", "kft",
    "spa", "zrt", "kk", "pte", "sdn", "bhd", "cv", "ug", "technologies",
    "technology", "the",
}


def fetch(url):
    req = urllib.request.Request(url, headers=UA)
    with urllib.request.urlopen(req, timeout=120) as r:
        return r.read().decode("utf-8", "replace")


def from_ieee(text):
    for row in csv.DictReader(io.StringIO(text)):
        asn = (row.get("Assignment") or "").strip()
        org = (row.get("Organization Name") or "").strip()
        if len(asn) == 6 and org:
            yield asn.upper(), org


def from_wireshark(text):
    for line in text.splitlines():
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        parts = line.split("\t")
        mac = parts[0].strip()
        if "/" in mac:  # 28/36-bit block, not an OUI
            continue
        hexs = mac.replace(":", "").replace("-", "").upper()
        if len(hexs) != 6:
            continue
        # Column 2 is the short name, column 3 (when present) the long one.
        org = (parts[2] if len(parts) > 2 and parts[2].strip() else parts[1]).strip()
        if org:
            yield hexs, org


def from_old_rust(path):
    """Last-resort fallback: parse the array literals of a previously
    generated oui_table.rs so a machine with no network access can still
    rebuild the blob from data already committed."""
    with open(path, encoding="utf-8") as f:
        text = f.read()
    prefixes = [int(x, 16) for x in re.findall(r"0x([0-9a-fA-F]{6}),", text.split("VENDOR_IDX", 1)[0])]
    rest = text.split("VENDOR_IDX", 1)[1]
    idx_block, vendors_block = rest.split("VENDORS", 1)
    idx = [int(x) for x in re.findall(r"(\d+),", idx_block.split("&[", 1)[1].split("];", 1)[0])]
    names = re.findall(r'"((?:[^"\\]|\\.)*)"', vendors_block)
    names = [n.replace('\\"', '"').replace("\\\\", "\\") for n in names]
    if not (prefixes and len(prefixes) == len(idx) and names):
        raise ValueError("could not parse old oui_table.rs")
    for hexs_int, vendor_i in zip(prefixes, idx):
        yield f"{hexs_int:06X}", names[vendor_i]


def titlecase(word):
    """Title-case a word, leaving short all-caps acronyms (HP, AVM) alone."""
    return "-".join(
        p if p.isupper() and len(p) <= 3 else p.capitalize() for p in word.split("-")
    )


def normalise(org):
    """"Apple, Inc." -> "Apple"; "Espressif Inc." -> "Espressif"."""
    org = re.sub(r"[(\[].*?[)\]]", " ", org)          # drop parentheticals
    org = re.sub(r"[,;.]+", " ", org)                 # "Co.,Ltd" -> "Co Ltd"
    org = org.replace('"', "").replace("\\", "").replace("�", "")
    words = [w for w in re.split(r"\s+", org) if w]
    while words and re.sub(r"[^a-z/]", "", words[-1].lower()) in SUFFIXES:
        words.pop()
    words = [titlecase(w) for w in words if w]
    name = " ".join(words).strip()
    if len(name) > MAX_LEN:  # trim on a word boundary where one is near enough
        cut = name[:MAX_LEN].rstrip()
        space = cut.rfind(" ")
        name = cut[:space] if space >= MAX_LEN - 8 else cut
    return name


def main():
    table, source = {}, None
    for label, parse, arg in (
        (IEEE, from_ieee, IEEE),
        (WIRESHARK, from_wireshark, WIRESHARK),
        (f"old table ({OLD_RUST})", from_old_rust, OLD_RUST),
    ):
        try:
            get = fetch(arg) if arg.startswith("http") else arg
            rows = list(parse(get))
        except Exception as e:  # noqa: BLE001 - any failure means "try the next one"
            print(f"{label}: {e}", file=sys.stderr)
            continue
        if len(rows) > 1000:
            table, source = dict(rows), label
            break
    if not table:
        sys.exit("no usable OUI source")

    vendors, index, prefixes = [], {}, []
    for hexs, org in sorted(table.items()):
        name = normalise(org)
        if not name:
            continue
        if name not in index:
            index[name] = len(vendors)
            vendors.append(name)
        prefixes.append((int(hexs, 16), index[name]))
    if len(vendors) > 0xFFFF:
        sys.exit(f"{len(vendors)} vendors do not fit a u16 index")

    names_bytes = [v.encode("utf-8") for v in vendors]
    offsets, pos = [], 0
    for nb in names_bytes:
        offsets.append(pos)
        pos += len(nb)
    strings_blob = b"".join(names_bytes)

    out = bytearray()
    out += b"OUI1"
    out += struct.pack("<II", len(prefixes), len(vendors))
    for p, _ in prefixes:
        out += struct.pack("<I", p)
    for _, i in prefixes:
        out += struct.pack("<H", i)
    out += struct.pack("<I", len(strings_blob))
    out += strings_blob
    for off in offsets:
        out += struct.pack("<I", off)

    with open(OUT, "wb") as f:
        f.write(out)
    print(
        f"{OUT}: {len(prefixes)} prefixes, {len(vendors)} vendors, "
        f"{len(out)} bytes, from {source}, retrieved {date.today()}"
    )


if __name__ == "__main__":
    main()
