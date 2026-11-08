#!/usr/bin/env python3
"""Losslessly intern frozen JSON oracle values. Never run terminal code or overwrite files.

Usage: intern-oracle-fixture.py pack ORIGINAL NEW_PACKED
       intern-oracle-fixture.py unpack PACKED NEW_ORIGINAL
The compact canonical reserialization must reproduce the original bytes exactly.
"""
import hashlib
import json
import pathlib
import sys

def pack(value, raw):
    nodes = []
    seen = {}
    schemas = {}

    def visit(value):
        pairs = None
        if isinstance(value, list):
            node = [1, [visit(item) for item in value]]
        elif isinstance(value, dict):
            pairs = [[visit(key), visit(item)] for key, item in value.items()]
            node = [2, pairs]
        else:
            node = [0, value]
        key = json.dumps(node, ensure_ascii=False, separators=(",", ":"))
        if key not in seen:
            if pairs is not None:
                schema = tuple(key_id for key_id, _ in pairs)
                if schema in schemas:
                    base, old = schemas[schema]
                    changes = [
                        [key_id, item] for (key_id, item), (_, previous)
                        in zip(pairs, old) if item != previous
                    ]
                    patch = [3, [base, changes]]
                    if len(json.dumps(patch)) < len(key):
                        node = patch
            seen[key] = len(nodes)
            nodes.append(node)
        result = seen[key]
        if pairs is not None:
            schemas[tuple(key_id for key_id, _ in pairs)] = (result, pairs)
        return result

    root = visit(value)
    return dict(
        format="jterm-oracle-intern-v1",
        original_sha256=hashlib.sha256(raw).hexdigest(),
        original_bytes=len(raw), root=root, nodes=nodes,
    )


def unpack(value):
    decoded = []
    for tag, payload in value["nodes"]:
        if tag == 0:
            item = payload
        elif tag == 1:
            item = [decoded[index] for index in payload]
        elif tag == 2:
            item = {decoded[key]: decoded[index] for key, index in payload}
        elif tag == 3:
            base, changes = payload
            item = dict(decoded[base])
            item.update({decoded[key]: decoded[index] for key, index in changes})
        else:
            raise ValueError(tag)
        decoded.append(item)
    return decoded[value["root"]]


def main():
    if len(sys.argv) != 4 or sys.argv[1] not in ("pack", "unpack"):
        raise SystemExit(__doc__)
    mode, source, target = sys.argv[1:]
    raw = pathlib.Path(source).read_bytes()
    value = json.loads(raw)
    canonical = lambda v: json.dumps(v, ensure_ascii=False, separators=(",", ":")).encode()
    if mode == "pack":
        result = pack(value, raw)
        decoded = unpack(result)
        assert decoded == value, "JSON values changed"
        assert canonical(decoded) == raw, "source must use canonical compact JSON"
        output = canonical(result)
    else:
        assert value["format"] == "jterm-oracle-intern-v1"
        output = canonical(unpack(value))
        assert len(output) == value["original_bytes"]
        assert hashlib.sha256(output).hexdigest() == value["original_sha256"]
    with pathlib.Path(target).open("xb") as file:
        file.write(output)
    print(f"{len(raw)} -> {len(output)} bytes; SHA-256 {hashlib.sha256(output).hexdigest()}")

if __name__ == "__main__":
    main()
