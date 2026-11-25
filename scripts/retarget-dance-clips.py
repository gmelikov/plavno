"""Retarget CMU subject 05 excerpts. Usage: python3 scripts/retarget-dance-clips.py SOURCE_DIR."""

from math import atan2, cos, degrees, radians, sin
from pathlib import Path
import struct
import sys

import numpy as np


def rotation(axes, values):
    matrix = np.eye(3)
    for axis, value in zip(axes.lower(), values, strict=True):
        c, s = cos(radians(value)), sin(radians(value))
        if axis == "x":
            turn = ((1, 0, 0), (0, c, -s), (0, s, c))
        elif axis == "y":
            turn = ((c, 0, s), (0, 1, 0), (-s, 0, c))
        else:
            turn = ((c, -s, 0), (s, c, 0), (0, 0, 1))
        matrix = np.asarray(turn) @ matrix
    return matrix


def skeleton(path):
    text = path.read_text()
    bones, hierarchy = text.split(":bonedata")[1].split(":hierarchy")
    parents = {}
    for row in hierarchy.splitlines():
        members = row.split()
        if len(members) > 1:
            parent, *children = members
            parents.update((child, parent) for child in children)
    result = []
    for block in bones.split("begin")[1:]:
        fields = {parts[0]: parts[1:] for row in block.splitlines() if (parts := row.split())}
        name = fields["name"][0]
        offset = np.array(fields["direction"], dtype=float) * float(fields["length"][0])
        *angles, order = fields["axis"]
        basis = rotation(order, map(float, angles))
        axes = "".join(name[-1] for name in fields.get("dof", []))
        result.append((name, parents[name], offset, basis, axes))
    return result


def frames(path):
    frame = None
    for row in path.read_text().splitlines():
        fields = row.split()
        if len(fields) == 1 and fields[0].isdigit():
            if frame is not None:
                yield frame
            frame = {}
        elif frame is not None and fields:
            frame[fields[0]] = list(map(float, fields[1:]))
    if frame is not None:
        yield frame


def pose(bones, frame):
    root = frame["root"]
    points = {"root": np.array(root[:3])}
    angles = {"root": rotation("xyz", root[3:])}
    for name, parent, offset, basis, axes in bones:
        angles[name] = angles[parent] @ basis @ rotation(axes, frame.get(name, [])) @ basis.T
        points[name] = points[parent] + angles[name] @ offset
    return points, angles


def wrap(angle):
    return (angle + np.pi) % (2 * np.pi) - np.pi


def yaw(matrix):
    return atan2(matrix[0, 2], matrix[2, 2])


def compile_clip(bones, source, start, end, steps, output):
    # Restrict the source's stage travel to the terminal-sized stage. Project its
    # dominant direction into screen X and use a shallower depth axis.
    poses = [pose(bones, frame) for frame in list(frames(source))[start : end + 1]]
    first, initial = poses[0]
    scale = 0.45 / (6.58 + 7.95)
    standing_y = max(points["root"][1] for points, _ in poses)
    horizontal = (poses[-1][0]["root"] - first["root"])[[0, 2]]
    path = horizontal / np.linalg.norm(horizontal)
    cross = np.array((-path[1], path[0]))

    def stage(point):
        displacement = (point - first["root"]) * scale
        x, z = displacement[[0, 2]]
        return np.array((0.66 * (path @ (x, z)), -displacement[1], 0.40 * (cross @ (x, z))))

    def channels(points, angles):
        root = stage(points["root"])
        root[1] = (standing_y - points["root"][1]) * scale
        chest = points["lowerneck"]
        chest_yaw = yaw(angles["thorax"])
        pelvis_yaw = yaw(angles["root"])
        facing = np.clip(0.54 * wrap(pelvis_yaw - yaw(initial["root"])), -1.44, 1.44)
        twist = np.clip(wrap(chest_yaw - pelvis_yaw), -0.55, 0.55)
        look = np.clip(wrap(yaw(angles["head"]) - chest_yaw), -0.55, 0.55)
        lower = angles["root"].T @ angles["lowerback"]
        lumbar = np.clip(-atan2(lower[1, 0], lower[0, 0]), -0.23, 0.23)
        pitch = np.clip(angles["thorax"][1, 2], -0.32, 0.32)
        palms, rolls = [], []
        for side in ("r", "l"):
            wrist = points[side + "wrist"]
            local = rotation("y", [-degrees(chest_yaw)]) @ ((wrist - chest) * scale)
            palms.extend((local[0], -local[1] - 0.022, np.clip(local[2] / 0.15, -1, 1)))
            sole = points[side + "toes"] - points[side + "tibia"]
            rolls.append(np.clip(atan2(-sole[1], np.linalg.norm(sole[[0, 2]])) - 0.15, -0.32, 0.58))
        return [*root, facing, twist, look, lumbar, pitch, *palms, *rolls]

    data = np.array([channels(points, angles) for points, angles in poses])
    with output.open("wb") as file:
        for time in np.linspace(0, len(data) - 1, 97):
            frame = int(time)
            blend = time - frame
            sample = data[frame] * (1 - blend) + data[min(frame + 1, len(data) - 1)] * blend
            file.write(struct.pack("<16h", *(round(value * 10000) for value in sample)))
        for lift, land, side in steps:
            points, angles = poses[land - start]
            ankle = stage(points[side + "tibia"])
            lifted = [p[side + "tibia"][1] for p, _ in poses[lift - start : land - start + 1]]
            height = np.clip((max(lifted) - max(lifted[0], lifted[-1])) * scale, 0.022, 0.15)
            facing = np.clip(0.54 * wrap(yaw(angles[side + "foot"]) - yaw(initial["root"])), -1.25, 1.25)
            file.write(struct.pack("<BHHhhhh", int(side == "l"),
                                   round(10000 * (lift - start) / (end - start)),
                                   round(10000 * (land - start) / (end - start)),
                                   round(10000 * ankle[0]), round(10000 * ankle[2]),
                                   round(10000 * facing), round(10000 * height)))


if __name__ == "__main__":
    source = Path(sys.argv[1])
    output = Path(__file__).resolve().parents[1] / "src" / "dance_clips"
    output.mkdir(exist_ok=True)
    bones = skeleton(source / "05.asf")
    compile_clip(bones, source / "05_12.amc", 1015, 1352,
                 [(1027, 1129, "l"), (1135, 1164, "r")], output / "soft.bin")
    compile_clip(bones, source / "05_03.amc", 0, 420,
                 [(6, 87, "r"), (91, 253, "l"), (260, 292, "r"),
                  (310, 349, "r"), (381, 416, "l")], output / "turn.bin")
