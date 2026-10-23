#!/usr/bin/env python3
"""check_locales.py -- the configuration window's strings (crates/symbiotic-explorer/ui/locales/*.json)
against each other: the same keys in every language; a plural string an object with an "other" form
(a language may have fewer forms than English: Chinese has only "other"); and the same {placeholders}
in every language's text for a key, so no language drops or invents a value. English is the reference.
"""
import json
import pathlib
import re
import sys

DIR = pathlib.Path(__file__).resolve().parent.parent / "crates/symbiotic-explorer/ui/locales"
PLACEHOLDER = re.compile(r"\{(\w+)\}")


def forms(v):
    return list(v.values()) if isinstance(v, dict) else [v]


def main():
    files = sorted(DIR.glob("*.json"))
    langs = {f.stem: json.loads(f.read_text(encoding="utf-8")) for f in files}
    ref = langs.get("en")
    if ref is None:
        sys.exit("no en.json")
    bad = []
    for lang, table in langs.items():
        for k in sorted(set(ref) - set(table)):
            bad.append(f"{lang}: missing {k}")
        for k in sorted(set(table) - set(ref)):
            bad.append(f"{lang}: {k} is not in en.json")
        for k, v in table.items():
            if isinstance(v, dict) and "other" not in v:
                bad.append(f"{lang}: {k} has no 'other' form")
            if not all(isinstance(x, str) for x in forms(v)):
                bad.append(f"{lang}: {k} is not text")
            if k in ref:
                want = set().union(*(PLACEHOLDER.findall(x) for x in forms(ref[k]) if isinstance(x, str)))
                for x in forms(v):
                    got = set(PLACEHOLDER.findall(x)) if isinstance(x, str) else set()
                    # A plural form may leave {n} out ("one instrument"); nothing else may differ.
                    if got - want or (want - got) - {"n"}:
                        bad.append(f"{lang}: {k} has {{{', '.join(sorted(got))}}}, en has {{{', '.join(sorted(want))}}}")
    for b in bad:
        print(b)
    if bad:
        sys.exit(1)
    print(f"{len(ref)} keys in {', '.join(sorted(langs))}, the same keys and placeholders")


if __name__ == "__main__":
    main()
