"""Native pcbnew worker for an atomic, closed-board Reference-field update.

The Rust caller owns the board-state gate, source revision checks, temporary
files, and atomic replacement. This worker never saves to the source path.
"""
from __future__ import annotations

import hashlib
import json
import math
import re
import sys
from pathlib import Path

import pcbnew as p


def fail(message: str) -> None:
    raise RuntimeError(message)


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def block_end(text: str, start: int) -> int:
    depth = 0
    quoted = False
    escaped = False
    for index in range(start, len(text)):
        char = text[index]
        if quoted:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == '"':
                quoted = False
            continue
        if char == '"':
            quoted = True
        elif char == "(":
            depth += 1
        elif char == ")":
            depth -= 1
            if depth == 0:
                return index + 1
    fail(f"unterminated S-expression block at {start}")


FOOTPRINT = re.compile(r"(?m)^[ \t]*\(footprint(?:\s|$)")
REFERENCE = re.compile(r'\(property\s+"Reference"\s+"([^"\\]*(?:\\.[^"\\]*)*)"')


def unescape(value: str) -> str:
    return value.replace(r'\"', '"').replace(r"\\", "\\")


def reference_blocks(data: bytes) -> dict[str, tuple[int, int]]:
    text = data.decode("utf-8")
    result: dict[str, tuple[int, int]] = {}
    seen_starts: list[int] = []
    for footprint_match in FOOTPRINT.finditer(text):
        footprint_start = text.find("(", footprint_match.start(), footprint_match.end())
        footprint_end = block_end(text, footprint_start)
        matches = list(REFERENCE.finditer(text, footprint_start, footprint_end))
        if len(matches) != 1:
            fail(f"footprint block at {footprint_start} has {len(matches)} Reference properties")
        match = matches[0]
        start = match.start()
        end = block_end(text, start)
        reference = unescape(match.group(1))
        if reference in result:
            fail(f"duplicate Reference property '{reference}'")
        result[reference] = (start, end)
        seen_starts.append(start)
    if [match.start() for match in REFERENCE.finditer(text)] != seen_starts:
        fail("Reference property exists outside a complete footprint block")
    return result


def normalized(data: bytes, requested: set[str]) -> bytes:
    text = data.decode("utf-8")
    blocks = reference_blocks(data)
    if not requested.issubset(blocks):
        fail("serialized board omitted one or more requested Reference blocks")
    selected = sorted((blocks[reference][0], blocks[reference][1], reference) for reference in requested)
    for start, end, reference in reversed(selected):
        text = text[:start] + f'(property "Reference" "{reference}" __NORMALIZED__)' + text[end:]
    return text.encode("utf-8")


def reference_immutable_snapshot(data: bytes, requested: set[str]) -> dict:
    """Preserve every serialized field property, including unknown future ones.

    Only numeric at/size/thickness leaves are omitted. This supplements native
    getters, which cannot establish completeness for all KiCad field metadata.
    It is a read-only comparison; CAD is written exclusively by pcbnew.
    """
    text = data.decode("utf-8")
    blocks = reference_blocks(data)
    result = {}
    token_pattern = re.compile(r'\s+|"(?:\\.|[^"\\])*"|[()]|[^\s()"]+')
    for reference in requested:
        if reference not in blocks:
            fail(f"missing serialized Reference '{reference}'")
        start, end = blocks[reference]
        block = text[start:end]
        tokens = []
        position = 0
        while position < len(block):
            match = token_pattern.match(block, position)
            if match is None:
                fail("invalid serialized Reference token")
            token = match.group()
            if not token.isspace():
                tokens.append(token)
            position = match.end()
        stack = [[]]
        for token in tokens:
            if token == "(":
                node = []
                stack[-1].append(node)
                stack.append(node)
            elif token == ")":
                if len(stack) == 1:
                    fail("unbalanced serialized Reference")
                stack.pop()
            else:
                stack[-1].append(token)
        if len(stack) != 1 or len(stack[0]) != 1:
            fail("incomplete serialized Reference")
        tree = stack[0][0]

        def omit_numeric_child(parent, name, counts, required):
            matches = [child for child in parent if isinstance(child, list) and child and child[0] == name]
            if len(matches) > 1 or (required and len(matches) != 1):
                fail(f"ambiguous serialized Reference geometry '{name}'")
            for child in matches:
                if len(child) - 1 not in counts:
                    fail(f"unsupported serialized Reference geometry '{name}'")
                try:
                    valid = all(isinstance(value, str) and math.isfinite(float(value)) for value in child[1:])
                except ValueError:
                    valid = False
                if not valid:
                    fail(f"non-numeric serialized Reference geometry '{name}'")
                parent.remove(child)

        omit_numeric_child(tree, "at", {2, 3}, True)
        effects = [child for child in tree if isinstance(child, list) and child and child[0] == "effects"]
        if len(effects) != 1:
            fail("serialized Reference requires exactly one effects block")
        fonts = [child for child in effects[0] if isinstance(child, list) and child and child[0] == "font"]
        if len(fonts) != 1:
            fail("serialized Reference requires exactly one font block")
        omit_numeric_child(fonts[0], "size", {2}, True)
        omit_numeric_child(fonts[0], "thickness", {1}, False)
        result[reference] = tree
    return result


def field_snapshot(field) -> dict:
    position = field.GetPosition()
    size = field.GetTextSize()
    return {
        "text": field.GetText(),
        "uuid": field.m_Uuid.AsString(),
        "visible": bool(field.IsVisible()),
        "force_visible": bool(field.IsForceVisible()),
        "keep_upright": bool(field.IsKeepUpright()),
        "knockout": bool(field.IsKnockout()),
        "locked": bool(field.IsLocked()),
        "multiline_allowed": bool(field.IsMultilineAllowed()),
        "layer": field.GetLayerName(),
        "mirrored": bool(field.IsMirrored()),
        "horizontal_justify": int(field.GetHorizJustify()),
        "vertical_justify": int(field.GetVertJustify()),
        "font_name": field.GetFontName(),
        "bold": bool(field.IsBold()),
        "italic": bool(field.IsItalic()),
        "hyperlink": field.GetHyperlink(),
        "field_id": int(field.GetId()),
        "x_nm": int(position.x),
        "y_nm": int(position.y),
        "rotation": float(field.GetTextAngleDegrees()),
        "size_x_nm": int(size.x),
        "size_y_nm": int(size.y),
        "stroke_nm": int(field.GetTextThickness()),
    }


GEOMETRY = {"x_nm", "y_nm", "rotation", "size_x_nm", "size_y_nm", "stroke_nm"}


def board_snapshot(board) -> dict[str, dict]:
    result: dict[str, dict] = {}
    for footprint in board.GetFootprints():
        reference = footprint.GetReference()
        if not reference or reference in result:
            fail(f"board has empty or duplicate footprint reference '{reference}'")
        result[reference] = field_snapshot(footprint.Reference())
    return result


def angle_equal(left: float, right: float) -> bool:
    delta = (left - right) % 360.0
    return math.isclose(delta, 0.0, abs_tol=1e-9) or math.isclose(delta, 360.0, abs_tol=1e-9)


def main() -> int:
    if len(sys.argv) != 6:
        fail("usage: worker source control-output candidate-output plan-json expected-sha256")
    source, control_path, candidate_path, plan_path = map(Path, sys.argv[1:5])
    expected_sha = sys.argv[5].lower()
    source_bytes = source.read_bytes()
    if hashlib.sha256(source_bytes).hexdigest() != expected_sha:
        fail("source SHA-256 changed before native pcbnew load")

    plan = json.loads(plan_path.read_text(encoding="utf-8"))
    rows = plan.get("placements")
    if not isinstance(rows, list) or not rows:
        fail("placements must be a non-empty array")
    requested = [row.get("reference") for row in rows]
    if any(not isinstance(reference, str) or not reference.strip() for reference in requested):
        fail("every placement requires a non-empty reference")
    if len(set(requested)) != len(requested):
        fail("placement references must be unique")
    requested_set = set(requested)

    control_board = p.LoadBoard(str(source))
    candidate_board = p.LoadBoard(str(source))
    source_fields = board_snapshot(control_board)
    if not requested_set.issubset(source_fields):
        fail("one or more requested footprint references are missing")
    footprints = {footprint.GetReference(): footprint for footprint in candidate_board.GetFootprints()}

    for row in rows:
        values = [row.get(key) for key in ("x", "y", "rotation", "size_x", "size_y", "stroke_width")]
        if any(not isinstance(value, (int, float)) or not math.isfinite(float(value)) for value in values):
            fail(f"placement '{row['reference']}' contains a non-finite or non-numeric value")
        if any(float(value) <= 0 for value in values[3:]):
            fail(f"placement '{row['reference']}' size and stroke must be positive")
        field = footprints[row["reference"]].Reference()
        field.SetPosition(p.VECTOR2I(p.FromMM(row["x"]), p.FromMM(row["y"])))
        field.SetTextAngleDegrees(row["rotation"])
        field.SetTextSize(p.VECTOR2I(p.FromMM(row["size_x"]), p.FromMM(row["size_y"])))
        field.SetTextThickness(p.FromMM(row["stroke_width"]))

    p.SaveBoard(str(control_path), control_board)
    p.SaveBoard(str(candidate_path), candidate_board)
    if sha256(source) != expected_sha:
        fail("source SHA-256 changed while native pcbnew was producing temporary outputs")

    control_fields = board_snapshot(p.LoadBoard(str(control_path)))
    candidate_fields = board_snapshot(p.LoadBoard(str(candidate_path)))
    if set(source_fields) != set(control_fields) or set(control_fields) != set(candidate_fields):
        fail("footprint reference set changed during native serialization")
    if source_fields != control_fields:
        fail("a no-op native save changed Reference-field semantics")

    by_reference = {row["reference"]: row for row in rows}
    changed_count = 0
    applied = []
    for reference, before in control_fields.items():
        after = candidate_fields[reference]
        if reference not in requested_set:
            if after != before:
                fail(f"unrequested Reference field '{reference}' changed")
            continue
        for key in before.keys() - GEOMETRY:
            if after[key] != before[key]:
                fail(f"Reference field '{reference}' changed immutable property '{key}'")
        row = by_reference[reference]
        expected = {
            "x_nm": int(p.FromMM(row["x"])),
            "y_nm": int(p.FromMM(row["y"])),
            "size_x_nm": int(p.FromMM(row["size_x"])),
            "size_y_nm": int(p.FromMM(row["size_y"])),
            "stroke_nm": int(p.FromMM(row["stroke_width"])),
        }
        for key, value in expected.items():
            if after[key] != value:
                fail(f"Reference field '{reference}' read back wrong {key}")
        if not angle_equal(after["rotation"], float(row["rotation"])):
            fail(f"Reference field '{reference}' read back wrong rotation")
        if any(after[key] != before[key] for key in GEOMETRY if key != "rotation") or not angle_equal(after["rotation"], before["rotation"]):
            changed_count += 1
        applied.append({
            "reference": reference,
            "x": p.ToMM(after["x_nm"]),
            "y": p.ToMM(after["y_nm"]),
            "rotation": after["rotation"],
            "size_x": p.ToMM(after["size_x_nm"]),
            "size_y": p.ToMM(after["size_y_nm"]),
            "stroke_width": p.ToMM(after["stroke_nm"]),
        })

    control_bytes = control_path.read_bytes()
    candidate_bytes = candidate_path.read_bytes()
    if reference_immutable_snapshot(source_bytes, set(source_fields)) != reference_immutable_snapshot(control_bytes, set(source_fields)):
        fail("a no-op native save changed serialized immutable Reference properties")
    if normalized(control_bytes, requested_set) != normalized(candidate_bytes, requested_set):
        fail("native candidate differs from the no-op control outside requested complete Reference blocks")
    if reference_immutable_snapshot(control_bytes, requested_set) != reference_immutable_snapshot(candidate_bytes, requested_set):
        fail("native candidate changed serialized immutable Reference properties")

    result = {
        "status": "PASS",
        "kicad_version": p.GetBuildVersion(),
        "source_sha256": expected_sha,
        "control_sha256": sha256(control_path),
        "candidate_sha256": sha256(candidate_path),
        "requested_count": len(rows),
        "changed_count": changed_count,
        "unchanged_count": len(rows) - changed_count,
        "placements": sorted(applied, key=lambda item: requested.index(item["reference"])),
        "normalized_non_reference_equal": True,
    }
    print(json.dumps(result, sort_keys=True))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"closed-board Reference worker failed: {error}", file=sys.stderr)
        raise SystemExit(1)
