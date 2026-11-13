#!/usr/bin/env python3
"""Bind Verus contracts to unchanged executable Rust bodies, failing on signature drift."""

import argparse
import json
import re
from pathlib import Path


def code_mask(source):
    """Keep offsets while removing comments and Rust string/character literals."""
    result = list(source)
    index = 0
    while index < len(source):
        end = index
        if source.startswith("//", index):
            end = source.find("\n", index)
            if end < 0:
                end = len(source)
        elif source.startswith("/*", index):
            depth, end = 1, index + 2
            while depth and end < len(source):
                if source.startswith("/*", end):
                    depth, end = depth + 1, end + 2
                elif source.startswith("*/", end):
                    depth, end = depth - 1, end + 2
                else:
                    end += 1
            if depth:
                raise ValueError("unterminated Rust comment")
        else:
            raw = re.match(r'(?:br|cr|r)(#*)"', source[index:])
            if raw:
                closing = '"' + raw.group(1)
                finish = source.find(closing, index + raw.end())
                if finish < 0:
                    raise ValueError("unterminated Rust raw string")
                end = finish + len(closing)
            elif source[index] == '"':
                end = index + 1
                while end < len(source) and source[end] != '"':
                    end += 2 if source[end] == "\\" else 1
                if end >= len(source):
                    raise ValueError("unterminated Rust string")
                end += 1
            elif source[index] == "'":
                character = re.match(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'", source[index:])
                if character:
                    end = index + character.end()
        if end > index:
            for offset in range(index, end):
                if result[offset] != "\n":
                    result[offset] = " "
            index = end
        else:
            index += 1
    return "".join(result)


def normalize(text):
    return re.sub(r"\s+", "", text)


def extract(source, name, signature):
    masked = code_mask(source)
    matches = list(re.finditer(r"\bfn\s+" + re.escape(name) + r"\s*\(", masked))
    if len(matches) != 1:
        raise ValueError(f"expected one production function: {name}")
    start = matches[0].start()
    opening = masked.find("{", start)
    if opening < 0 or normalize(source[start:opening]) != normalize(signature):
        raise ValueError(f"production signature changed: {name}")
    depth, end = 1, opening + 1
    while depth and end < len(masked):
        depth += (masked[end] == "{") - (masked[end] == "}")
        end += 1
    if depth:
        raise ValueError(f"unterminated production body: {name}")
    return source[opening:end]


def generate(root, manifest):
    output = [
        "// Generated from current production source. Do not edit.",
    ]
    names = set()
    for module in manifest["modules"]:
        name = module["module"]
        if not re.fullmatch(r"[a-z][a-z0-9_]*", name) or name in names:
            raise ValueError("invalid or duplicate proof module")
        names.add(name)
        source = (root / module["source"]).read_text()
        output.append(f"mod {name} {{ use vstd::prelude::*; verus! {{")
        for function in module["functions"]:
            body = extract(source, function["name"], function["signature"])
            if not function.get("ensures"):
                if function.get("verus") is not False:
                    raise ValueError("missing Verus contract")
                continue
            arguments, result = function["signature"].rsplit("->", 1)
            output.append(f"{arguments} -> (result: {result.strip()})")
            output.append("ensures\n" + ",\n".join(function["ensures"]) + ",")
            output.append(body)
        output.append("} }")
    output.append("fn main() {}")
    return "\n".join(output) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    manifest = json.loads((args.root / "verification/contracts.json").read_text())
    args.output.write_text(generate(args.root, manifest))


if __name__ == "__main__":
    main()
