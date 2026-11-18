#!/usr/bin/env python3
"""Bind Verus contracts to unchanged executable Rust bodies, failing on signature drift."""

import argparse
import json
import re
from pathlib import Path


RAW_LITERAL = re.compile(r'(?:br|cr|r)(#*)"')
CHARACTER_LITERAL = re.compile(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'")


def block_comment_end(source, index):
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
    return end


def comment_end(source, index):
    if source.startswith("//", index):
        end = source.find("\n", index)
        return len(source) if end < 0 else end
    if source.startswith("/*", index):
        return block_comment_end(source, index)
    return None


def string_end(source, index):
    end = index + 1
    while end < len(source) and source[end] != '"':
        end += 2 if source[end] == "\\" else 1
    if end >= len(source):
        raise ValueError("unterminated Rust string")
    return end + 1


def literal_end(source, index):
    raw = RAW_LITERAL.match(source, index)
    if raw:
        closing = '"' + raw.group(1)
        finish = source.find(closing, raw.end())
        if finish < 0:
            raise ValueError("unterminated Rust raw string")
        return finish + len(closing)
    if source[index] == '"':
        return string_end(source, index)
    if source[index] == "'":
        character = CHARACTER_LITERAL.match(source, index)
        if character:
            return character.end()
    return None


def code_mask(source):
    """Keep offsets while removing comments and Rust string/character literals."""
    result = list(source)
    index = 0
    while index < len(source):
        end = comment_end(source, index)
        if end is None:
            end = literal_end(source, index)
        if end is None:
            index += 1
            continue
        for offset in range(index, end):
            if result[offset] != "\n":
                result[offset] = " "
        index = end
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


class DeclarationScope:
    """Track module item boundaries without interpreting masked Rust trivia."""

    OPENING = {"(": ")", "[": "]", "{": "}"}

    def __init__(self):
        self.stack = []
        self.item_start = 0

    def advance(self, index, token):
        if token in self.OPENING:
            self.stack.append(self.OPENING[token])
        elif token in ")]}":
            if not self.stack or self.stack.pop() != token:
                raise ValueError("unbalanced production declaration scope")
            if not self.stack and token == "}":
                self.item_start = index + 1
        elif token == ";" and not self.stack:
            self.item_start = index + 1


def module_item_start(masked, start):
    """Locate a declaration in the file's own unconditional module."""
    scope = DeclarationScope()
    for index, token in enumerate(masked[:start]):
        if token == "#" and not scope.stack and re.match(r"#\s*!\s*\[", masked[index:start]):
            raise ValueError("module attributes prevent declaration binding")
        scope.advance(index, token)
    if scope.stack:
        raise ValueError("declaration must belong to the file module")
    return scope.item_start


def constant_module_scope(masked, start):
    """Admit an unconditional declaration in the file's own module only."""
    prefix = masked[module_item_start(masked, start):start].strip()
    if not re.fullmatch(r"(?:pub(?:\s*\([^()]*\))?)?", prefix):
        raise ValueError("constant must be an unconditional module declaration")


def without_comments(source):
    """Remove comments while preserving literal contents in checked attributes."""
    result = list(source)
    index = 0
    while index < len(source):
        end = comment_end(source, index)
        if end is not None:
            result[index:end] = " " * (end - index)
            index = end
        else:
            index = literal_end(source, index) or index + 1
    return "".join(result)


def extract_struct(source, declaration):
    """Bind exact production fields; admit only the explicitly reviewed attributes."""
    name = declaration["name"]
    if not re.fullmatch(r"[A-Z][a-zA-Z0-9_]*", name):
        raise ValueError("invalid production struct name")
    masked = code_mask(source)
    matches = list(re.finditer(r"\bstruct\s+" + re.escape(name) + r"\b", masked))
    if len(matches) != 1:
        raise ValueError(f"expected one production struct: {name}")
    start = matches[0].start()
    prefix = without_comments(source[module_item_start(masked, start):start])
    prefix = re.sub(r"\bpub(?:\s*\([^()]*\))?\s*$", "", prefix).strip()
    if normalize(prefix) != normalize("".join(declaration.get("attributes", []))):
        raise ValueError(f"production struct attributes changed: {name}")
    opening = masked.find("{", start)
    closing = masked.find("}", opening)
    if opening < 0 or closing < 0:
        raise ValueError(f"unterminated production struct: {name}")
    actual = source[start:closing + 1]
    if normalize(without_comments(actual)) != normalize(declaration["declaration"]):
        raise ValueError(f"production struct fields changed: {name}")
    return actual


def bound_structs(root, module):
    names = set()
    for declaration in module.get("types", []):
        if declaration["name"] in names:
            raise ValueError("duplicate proof struct")
        names.add(declaration["name"])
        source = (root / declaration.get("source", module["source"])).read_text()
        yield declaration["name"], extract_struct(source, declaration)


def generate_kani_types(root, manifest):
    output = ["// Generated from current production fields. Do not edit."]
    names = set()
    for module in manifest["modules"]:
        for name, declaration in bound_structs(root, module):
            if name in names:
                raise ValueError("ambiguous Kani proof struct")
            names.add(name)
            output.append(declaration)
    return "\n".join(output) + "\n"


def extract_constant(source, name, signature):
    """Copy one scalar constant declaration, retaining its actual initializer."""
    if not re.fullmatch(r"[A-Z][A-Z0-9_]*", name):
        raise ValueError("invalid production constant name")
    masked = code_mask(source)
    matches = list(re.finditer(r"\bconst\s+" + re.escape(name) + r"\s*:", masked))
    if len(matches) != 1:
        raise ValueError(f"expected one production constant: {name}")
    start = matches[0].start()
    constant_module_scope(masked, start)
    assignment = masked.find("=", start)
    if assignment < 0 or normalize(source[start:assignment]) != normalize(signature):
        raise ValueError(f"production constant signature changed: {name}")
    end = masked.find(";", assignment)
    if end < 0:
        raise ValueError(f"unterminated production constant: {name}")
    # The manifest supports scalar expressions. A block can contain its own
    # declarations; refusing it prevents taking an inner semicolon as the end.
    if any(token in masked[assignment:end] for token in "{}"):
        raise ValueError(f"unsupported production constant initializer: {name}")
    return source[start:end + 1]


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
        output.extend(declaration for _, declaration in bound_structs(root, module))
        constants = set()
        for constant in module.get("constants", []):
            if constant["name"] in constants:
                raise ValueError("duplicate proof constant")
            constants.add(constant["name"])
            output.append(extract_constant(source, constant["name"], constant["signature"]))
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
