"""Score sparse river/lake brush annotations against exported combined maps.

Optional dependencies: python -m pip install numpy pillow
Mask and maps must have exactly the same resolution and projection. Paint river
#3ae1cd, lake #99cacd, or select brushes with --river-color / --lake-color.
Leave unannotated pixels transparent or black. Nonzero alpha counts as paint.
Marks outside reference inland water are ignored. This
reports agreement with supplied annotations, not accuracy on an entire world.
"""
import argparse
import json
from pathlib import Path
import struct

import numpy as np
from PIL import Image

RIVER = ((58, 225, 205), (234, 215, 160))
LAKE = ((153, 202, 205), (203, 185, 132))


def classes(rgb):
    result = np.zeros(rgb.shape[:2], dtype=np.uint8)
    for label, colors in ((1, RIVER), (2, LAKE)):
        for color in colors:
            result[np.all(rgb == color, axis=2)] = label
    return result


def metrics(truth, predicted):
    table = np.bincount(truth.astype(np.int64) * 3 + predicted, minlength=9).reshape(3, 3)
    recalls = [float(table[k, k] / table[k].sum()) if table[k].sum() else None for k in (1, 2)]
    return {
        "sample_count": len(truth),
        "river_recall": recalls[0], "lake_recall": recalls[1],
        "balanced_accuracy": sum(recalls) / 2 if all(v is not None for v in recalls) else None,
        "accuracy": float(np.mean(truth == predicted)),
        "river_as_river": int(table[1, 1]), "river_as_lake": int(table[1, 2]),
        "lake_as_river": int(table[2, 1]), "lake_as_lake": int(table[2, 2]),
        "unavailable_predictions": int(table[1:, 0].sum()),
    }


def brush_color(value):
    value = value.removeprefix("#")
    if len(value) != 6:
        raise argparse.ArgumentTypeError("brush colour must be six hexadecimal digits, e.g. ff0000")
    if any(char not in "0123456789abcdefABCDEF" for char in value):
        raise argparse.ArgumentTypeError("brush colour must contain hexadecimal digits")
    return tuple(int(value[i:i+2], 16) for i in (0, 2, 4))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--mask", type=Path, required=True)
    parser.add_argument("--reference-map", type=Path, required=True)
    parser.add_argument("--reference-data", type=Path, required=True, help="water_regions.bin for the reference map")
    parser.add_argument("--map-scale", type=int, default=8)
    parser.add_argument("--river-color", type=brush_color, default=RIVER[0], help="river brush RGB, default 3ae1cd")
    parser.add_argument("--lake-color", type=brush_color, default=LAKE[0], help="lake brush RGB, default 99cacd")
    parser.add_argument("--map", type=Path, action="append", required=True, help="candidate combined PNG; repeat to compare")
    parser.add_argument("--output", type=Path, help="optional JSON report")
    args = parser.parse_args()
    if args.map_scale <= 0:
        parser.error("--map-scale must be positive")
    if args.river_color == args.lake_color:
        parser.error("river and lake brush colours must differ")
    with args.reference_data.open("rb") as file:
        header = file.read(80)
    if len(header) < 40 or header[:8] != b"MCWATER\0":
        parser.error("reference data is not a supported water_regions.bin")
    x0, z0, x1, z1 = struct.unpack_from(">4i", header, 24)
    width, height = (x1-x0)//args.map_scale+1, (z1-z0)//args.map_scale+1
    with Image.open(args.reference_map) as image:
        reference = classes(np.array(image.convert("RGB")))
    panel = reference.shape[1] - width
    if panel < 0 or reference.shape[0] != height:
        parser.error("reference PNG dimensions disagree with the binary bounds and scale")
    reference[:, :panel] = 0
    with Image.open(args.mask) as image:
        mask = np.array(image.convert("RGBA"))
    if mask.shape[:2] != reference.shape:
        parser.error("mask and reference resolution must match exactly; no automatic rescaling or shifting")
    paint = np.zeros(reference.shape, dtype=np.uint8)
    for label, color in ((1, args.river_color), (2, args.lake_color)):
        paint[(mask[:, :, 3] > 0) & np.all(mask[:, :, :3] == color, axis=2)] = label
    valid = (paint > 0) & (reference > 0)
    truth = paint[valid]
    if not len(truth):
        parser.error("no recognized annotation pixels overlap reference inland water")
    report = {
        "sample_count": int(valid.sum()),
        "river_labels": int((truth == 1).sum()), "lake_labels": int((truth == 2).sum()),
        "ignored_paint_outside_inland_water": int(((paint > 0) & ~valid).sum()),
        "brush_colors": {"river": args.river_color, "lake": args.lake_color},
        "projection": {"legend_width": panel, "blocks_per_pixel": args.map_scale, "world_bounds": [x0,z0,x1,z1]},
        "method": "Fixed reference river/lake pixels only. No alignment changes, dilation, filled annotations or background agreement. Missing candidate classes count as errors. Adjacent trace pixels are correlated; this is annotation agreement, not independent whole-world accuracy.",
        "results": [],
    }
    for path in args.map:
        with Image.open(path) as image:
            candidate = classes(np.array(image.convert("RGB")))
        if candidate.shape != reference.shape:
            parser.error(f"candidate resolution differs: {path}")
        report["results"].append({"map": path.as_posix(), **metrics(truth, candidate[valid])})
    encoded = json.dumps(report, indent=2)
    if args.output:
        args.output.write_text(encoded + "\n", encoding="utf-8")
    print(encoded)


if __name__ == "__main__":
    main()
