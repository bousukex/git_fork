#!/usr/bin/env python3
"""Writes packaging/THIRD_PARTY_LICENSES: the license texts of what the
pengitto executable is built from (the Rust crates linked into it on x86_64
Linux, and the assets they embed), from Cargo.lock and the crates' sources.

  scripts/third-party.py           write it
  scripts/third-party.py --check   fail if it isn't what would be written

The crates are those `cargo tree` gives for the x86_64 Linux build (normal
dependencies, no proc-macros). Each needs a license text: its files
(LICENSE*, COPYING*, NOTICE*…), else an override below; and the texts must
match what the crate declares (an MIT crate with an MIT text, an exception
with its text…). The embedded assets need the notice of each of their
parts. Anything missing fails the run.
Also writes packaging/APPIMAGE_NOTICES (the AppImage runtime's).
"""

import json
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LICENSES = os.path.join(ROOT, "packaging", "licenses")
OUT = os.path.join(ROOT, "packaging", "THIRD_PARTY_LICENSES")
APPIMAGE_OUT = os.path.join(ROOT, "packaging", "APPIMAGE_NOTICES")

# Crates whose packages carry no license file: each is offered under
# Apache-2.0 (alone or as an option), whose text is standard.
APACHE_ONLY_TEXT = [
    "accesskit", "accesskit_atspi_common", "accesskit_consumer", "accesskit_unix", "accesskit_winit",
    "ecolor", "eframe", "egui", "egui-wgpu", "egui-winit", "egui_extras", "emath", "epaint",
    "epaint_default_fonts", "profiling", "spirv",
]

# Assets embedded with a crate: what they are, their notices' files, and
# what each of their parts' notices says (all needed).
ASSETS = {
    "epaint_default_fonts": (
        "egui's default fonts: Ubuntu Light (Ubuntu Font Licence), Hack (MIT, Bitstream Vera),"
        " Noto Emoji (OFL-1.1), emoji-icon-font (MIT)",
        lambda d: sorted(os.path.join(d, "fonts", f) for f in os.listdir(os.path.join(d, "fonts")) if f.endswith(".txt")),
        [
            ("Ubuntu Light", ["UBUNTU FONT LICENCE Version 1.0"]),
            ("Hack", ["Source Foundry Authors", "Bitstream Vera", "Permission is hereby granted, free of charge"]),
            ("Noto Emoji", ["SIL OPEN FONT LICENSE Version 1.1"]),
            ("emoji-icon-font", ["Copyright (c) 2014 John Slegers", "Permission is hereby granted, free of charge"]),
        ],
    ),
    "syntect": (
        "syntect's default syntax definitions, built from Sublime Text's Packages",
        lambda d: [os.path.join(LICENSES, "sublime-packages.txt")],
        [
            ("Sublime Text's Packages", ["Permission to copy, use, modify, sell and distribute this software is granted"]),
            ("its YAML syntax", ["Copyright (c) 2015 FichteFoll"]),
            ("its C# syntax", ["Copyright (c) 2016 Sublime Text HQ Pty"]),
            ("its Rust package", ["Rust/LICENSE.txt", "Permission is hereby granted, free of charge"]),
        ],
    ),
}

# What a text of each license says, and what it doesn't (to tell texts of
# alike licenses apart: BSD-2 has no clause on endorsing, 0BSD no
# condition…), to check texts against what the crates declare.
SIGNS = {
    "MIT": (["Permission is hereby granted, free of charge", "shall be included in all copies"], []),
    "Apache-2.0": (["Apache License", "TERMS AND CONDITIONS FOR USE, REPRODUCTION, AND DISTRIBUTION"], []),
    "BSD-2-Clause": (["Redistribution and use in source and binary forms"], ["endorse or promote", "advertising materials"]),
    "BSD-3-Clause": (["Redistribution and use in source and binary forms", "endorse or promote"], ["advertising materials"]),
    "ISC": (["Permission to use, copy, modify, and/or distribute", "provided that the above copyright notice"], []),
    "0BSD": (["Permission to use, copy, modify, and/or distribute"], ["provided that the above copyright notice"]),
    "Zlib": (["Permission is granted to anyone to use this software for any purpose", "must be plainly marked"], []),
    "Unicode-3.0": (["UNICODE LICENSE V3"], []),
    "Unlicense": (["This is free and unencumbered software released into the public domain"], []),
    "CC0-1.0": (["CC0 1.0 Universal"], []),
    "OFL-1.1": (["SIL OPEN FONT LICENSE", "Version 1.1"], []),
    "Ubuntu-font-1.0": (["UBUNTU FONT LICENCE", "Version 1.0"], []),
    "GPL-2.0-only": (["GNU GENERAL PUBLIC LICENSE", "Version 2, June 1991"], []),
}

# The same for license exceptions (`… WITH <exception>`).
EXCEPTIONS = {
    "LLVM-exception": ["LLVM Exceptions to the Apache 2.0 License"],
}

LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|notice|unlicense|copyright)([-_.].*)?$", re.I)


def metadata():
    out = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--filter-platform", "x86_64-unknown-linux-gnu"],
        cwd=ROOT, check=True, capture_output=True,
    ).stdout
    return json.loads(out)


# The build the list is for: pengitto's normal dependencies on x86_64 Linux, with
# the features that build has (not those only tests turn on), and no
# proc-macros (they only run while compiling).
CARGO_TREE = [
    "cargo", "tree", "--locked", "-p", "pengitto", "--target", "x86_64-unknown-linux-gnu",
    "-e", "normal,no-proc-macro", "--prefix", "none", "--format", "{p}",
]


def linked_crates(meta):
    """The crates linked into pengitto (cargo tree's), without the workspace's."""
    out = subprocess.run(CARGO_TREE, cwd=ROOT, check=True, capture_output=True, text=True).stdout
    wanted = set()
    for line in out.splitlines():
        m = re.match(r"(\S+) v(\S+)", line)
        if m:
            wanted.add((m.group(1), m.group(2)))
    packages = {(p["name"], p["version"]): p for p in meta["packages"]}
    missing = sorted(w for w in wanted if w not in packages)
    if missing:
        raise SystemExit(f"cargo tree has crates cargo metadata doesn't: {missing}")
    pengitto = next(p for p in meta["packages"] if p["name"] == "pengitto" and p["source"] is None)
    crates = sorted((packages[w] for w in wanted if packages[w]["source"] is not None), key=lambda p: (p["name"], p["version"]))
    return pengitto, crates


def read(path):
    with open(path, encoding="utf-8", errors="replace") as f:
        text = f.read()
    return "\n".join(line.rstrip() for line in text.replace("\r\n", "\n").split("\n")).strip("\n") + "\n"


def texts_of(p):
    d = os.path.dirname(p["manifest_path"])
    files = sorted(os.path.join(d, f) for f in os.listdir(d) if LICENSE_FILE.match(f) and os.path.isfile(os.path.join(d, f)))
    sub = os.path.join(d, "LICENSES")
    if os.path.isdir(sub):
        files += sorted(os.path.join(sub, f) for f in os.listdir(sub))
    if p.get("license_file"):
        lf = os.path.join(d, p["license_file"])
        if lf not in files:
            files.append(lf)
    texts = [(os.path.relpath(f, d), read(f)) for f in files]
    if not texts and p["name"] in APACHE_ONLY_TEXT:
        texts = [(STANDARD_APACHE, read(os.path.join(LICENSES, "Apache-2.0.txt")))]
    return texts


STANDARD_APACHE = "Apache-2.0 (for crates with no license file in their package: the standard text)"


def copyright_of(p, texts):
    """For a crate with only the standard text: whose it is (by its
    Cargo.toml)."""
    if [title for title, _ in texts] != [STANDARD_APACHE]:
        return ""
    return f" — copyright: its authors, {', '.join(p['authors']) or 'none listed'}"


def asset_texts(p):
    """The notices of the assets embedded with the crate `p`."""
    if p["name"] not in ASSETS:
        return []
    d = os.path.dirname(p["manifest_path"])
    _, files, _ = ASSETS[p["name"]]
    # Ours by their place in the repository, the crate's in the crate.
    return [(os.path.relpath(f, ROOT) if f.startswith(LICENSES) else os.path.relpath(f, d), read(f)) for f in files(d)]


def flat(text):
    """`text` for matching: lower case, any run of white space one space."""
    return " ".join(text.lower().split())


def says(text, required, forbidden=()):
    t = flat(text)
    return all(flat(s) in t for s in required) and not any(flat(s) in t for s in forbidden)


def missing_parts(name, texts):
    """The parts of the crate `name`'s assets without their notice."""
    if name not in ASSETS:
        return []
    return [part for part, signs in ASSETS[name][2] if not any(says(t, signs) for _, t in texts)]


def satisfied(expr, texts):
    """Whether the texts cover the SPDX expression (an OR needs one side,
    an AND both; a license WITH an exception needs the exception's text
    too)."""
    expr = expr.replace("/", " OR ")
    tokens = re.findall(r"\(|\)|[A-Za-z0-9.+-]+", expr)
    pos = 0

    def atom():
        nonlocal pos
        if tokens[pos] == "(":
            pos += 1
            v = disj()
            pos += 1
            return v
        lic = tokens[pos]
        pos += 1
        sign = SIGNS.get(lic)
        if sign is None:
            raise SystemExit(f"unknown license {lic!r} in {expr!r}: add how its text looks to SIGNS")
        v = any(says(t, *sign) for _, t in texts)
        if pos < len(tokens) and tokens[pos] == "WITH":
            exception = tokens[pos + 1]
            pos += 2
            exc = EXCEPTIONS.get(exception)
            if exc is None:
                raise SystemExit(f"unknown exception {exception!r} in {expr!r}: add how its text looks to EXCEPTIONS")
            v = v and any(says(t, exc) for _, t in texts)
        return v

    def conj():
        nonlocal pos
        v = atom()
        while pos < len(tokens) and tokens[pos] == "AND":
            pos += 1
            v = atom() and v
        return v

    def disj():
        nonlocal pos
        v = conj()
        while pos < len(tokens) and tokens[pos] == "OR":
            pos += 1
            v = conj() or v
        return v

    return disj()


def third_party(meta):
    pengitto, crates = linked_crates(meta)
    problems, rows, texts = [], [], {}
    for p in crates:
        found = texts_of(p)
        if not found:
            problems.append(f"{p['name']} {p['version']}: no license text (add an override)")
            continue
        assets = asset_texts(p)
        # (A crate's declaration may cover its assets: the fonts' OFL…)
        if not satisfied(p["license"] or "", found + assets):
            problems.append(f"{p['name']} {p['version']}: its texts don't match {p['license']!r}")
        for part in missing_parts(p["name"], assets):
            problems.append(f"{p['name']} {p['version']}: no notice for {part} (of its assets)")
        refs = []
        for title, text in found + assets:
            entry = texts.setdefault(text, {"n": len(texts) + 1, "titles": set(), "users": []})
            entry["titles"].add(title)
            entry["users"].append(f"{p['name']} {p['version']}")
            refs.append(entry["n"])
        rows.append((p, sorted(set(refs)), copyright_of(p, found)))
    names = {p["name"] for p in crates}
    for name in sorted(set(APACHE_ONLY_TEXT) - names):
        problems.append(f"{name}: in APACHE_ONLY_TEXT but not linked (remove it)")
    if problems:
        raise SystemExit("third-party licenses incomplete:\n  " + "\n  ".join(problems))
    out = [
        f"THIRD-PARTY LICENSES — pengitto {pengitto['version']}",
        "",
        "The pengitto executable is built from the Rust crates below (those linked into",
        "it on x86_64 Linux) and the assets they embed. Each is listed with the",
        "license it declares and the texts that go with it, under LICENSE TEXTS.",
        "Generated by scripts/third-party.py from Cargo.lock; don't edit.",
        "",
        "CRATES",
    ]
    for p, refs, holders in rows:
        out.append(f"  {p['name']} {p['version']} — {p['license']} — texts {', '.join(map(str, refs))}{holders}")
    out += ["", "ASSETS"]
    for p, _, _ in rows:
        if p["name"] in ASSETS:
            out.append(f"  {ASSETS[p['name']][0]} (with {p['name']} {p['version']})")
    out += ["", "LICENSE TEXTS"]
    for text, e in sorted(texts.items(), key=lambda kv: kv[1]["n"]):
        out += ["", f"==== [{e['n']}] {' / '.join(sorted(e['titles']))}", f"Used by: {', '.join(e['users'])}", "", text.rstrip("\n")]
    return "\n".join(out) + "\n"


# What the pinned runtime is made of (its versions: those of the Alpine
# Linux packages it was built with; zlib's, zstd's and squashfuse's are in
# the runtime's own strings too).
APPIMAGE_PARTS = [
    ("type2-runtime-LICENSE.txt", "The AppImage runtime (type2-runtime, continuous build of commit 8f39b89e), MIT"),
    ("musl-1.2.5-COPYRIGHT.txt", "musl libc 1.2.5 (statically linked into the runtime), MIT"),
    ("mimalloc-2.1.7-LICENSE.txt", "mimalloc 2.1.7 (the runtime's memory allocator, statically linked), MIT"),
    ("libfuse-3.15.0-LICENSE.txt", "libfuse 3.15.0 (statically linked into the runtime, with the runtime's patch to mount.c)"),
    ("libfuse-3.15.0-LGPL2.txt", "libfuse 3.15.0: the GNU Lesser General Public License, version 2.1"),
    ("squashfuse-0.5.2-LICENSE.txt", "squashfuse 0.5.2 (statically linked into the runtime), BSD-2-Clause"),
    ("zstd-1.5.6-LICENSE.txt", "zstd 1.5.6 (statically linked into the runtime), BSD-3-Clause"),
    ("zlib-1.3.2-LICENSE.txt", "zlib 1.3.2 (statically linked into the runtime), Zlib"),
]


def appimage_notices():
    out = [
        "NOTICES FOR THE APPIMAGE",
        "",
        "Besides pengitto (see THIRD_PARTY_LICENSES), the AppImage file contains the",
        "AppImage runtime, which mounts the image when it starts. The runtime",
        "is the pinned build in packaging/appimage/tools.lock; its sources are at",
        "https://github.com/AppImage/type2-runtime (and those of what it links",
        "statically at the versions below). Generated by scripts/third-party.py.",
    ]
    for name, title in APPIMAGE_PARTS:
        out += ["", f"==== {title}", "", read(os.path.join(LICENSES, "appimage", name)).rstrip("\n")]
    return "\n".join(out) + "\n"


def main():
    check = "--check" in sys.argv[1:]
    wanted = {OUT: third_party(metadata()), APPIMAGE_OUT: appimage_notices()}
    stale = []
    for path, text in wanted.items():
        current = open(path, encoding="utf-8").read() if os.path.exists(path) else None
        if current == text:
            continue
        if check:
            stale.append(os.path.relpath(path, ROOT))
        else:
            with open(path, "w", encoding="utf-8") as f:
                f.write(text)
    if stale:
        raise SystemExit(f"out of date (run scripts/third-party.py): {', '.join(stale)}")


if __name__ == "__main__":
    main()
