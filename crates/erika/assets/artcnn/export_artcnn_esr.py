#!/usr/bin/env python3
"""Export an ArtCNN ONNX model (C or R series) to Erika's `.esr` network format.

Usage:
    python3 export_artcnn_esr.py ArtCNN_R8F64.onnx artcnn_r8f64.esr \
        [--name artcnn_r8f64] [--test-vector DIR]

The `.esr` layout is defined by `src/renderer/sr/model.rs` (`SrModel::to_bytes`).
Supported ONNX ops: Conv (3x3, SAME), Relu, Sigmoid+Mul (SiLU), Add (residual),
DepthToSpace (DCR, blocksize 2) and Clip. Weights are stored as float16.
"""

import argparse
import struct
import sys

import numpy as np
import onnx
from onnx import numpy_helper

MAGIC = 0x4D525345  # "ESRM"
VERSION = 1


def convert(model, name):
    graph = model.graph
    inits = {i.name: numpy_helper.to_array(i).astype(np.float32) for i in graph.initializer}
    input_name = graph.input[0].name

    tensors = [1]  # IR tensor channel counts
    ids = {input_name: 0}  # ONNX tensor name -> IR tensor id
    layers = []  # dicts
    by_output = {}  # IR tensor id -> layer dict
    head = None
    clamp = False

    nodes = list(graph.node)
    for node in nodes:
        op = node.op_type
        if op == "Conv":
            weight = inits[node.input[1]]
            bias = inits[node.input[2]]
            out_ch, in_ch, kh, kw = weight.shape
            assert kh == kw and kh % 2 == 1, weight.shape
            attrs = {a.name: onnx.helper.get_attribute_value(a) for a in node.attribute}
            assert attrs.get("strides", [1, 1]) == [1, 1]
            pad = attrs.get("auto_pad", b"").decode() if isinstance(attrs.get("auto_pad"), bytes) else attrs.get("auto_pad", "")
            assert pad in ("SAME_UPPER", "SAME_LOWER") or attrs.get("pads") == [kh // 2] * 4, attrs
            source = ids[node.input[0]]
            assert tensors[source] == in_ch
            tid = len(tensors)
            tensors.append(out_ch)
            layer = {
                "output": tid,
                "inputs": [(source, 0, kh, weight.reshape(-1))],
                "bias": bias,
                "residual": None,
                "activation": 0,
            }
            layers.append(layer)
            by_output[tid] = layer
            ids[node.output[0]] = tid
        elif op == "Relu":
            tid = ids[node.input[0]]
            assert by_output[tid]["activation"] == 0
            by_output[tid]["activation"] = 1
            ids[node.output[0]] = tid
        elif op == "Sigmoid":
            ids[node.output[0]] = ("sigmoid", ids[node.input[0]])
        elif op == "Mul":
            a, b = (ids[x] for x in node.input)
            if isinstance(b, tuple):
                a, b = b, a
            assert isinstance(a, tuple) and a[1] == b, node
            assert by_output[b]["activation"] == 0
            by_output[b]["activation"] = 2  # SiLU
            ids[node.output[0]] = b
        elif op == "Add":
            a, b = (ids[x] for x in node.input)
            # The conv whose output is only consumed by this Add absorbs it.
            target, other = (a, b) if a in by_output and by_output[a]["residual"] is None and by_output[a]["activation"] == 0 and a > b else (b, a)
            layer = by_output[target]
            assert layer["activation"] == 0 and layer["residual"] is None, node
            layer["residual"] = other
            ids[node.output[0]] = target
        elif op == "DepthToSpace":
            attrs = {a.name: onnx.helper.get_attribute_value(a) for a in node.attribute}
            assert attrs["blocksize"] == 2
            mode = attrs.get("mode", b"DCR")
            mode = mode.decode() if isinstance(mode, bytes) else mode
            assert mode == "DCR", mode
            head = ids[node.input[0]]
            assert tensors[head] == 4
            ids[node.output[0]] = ("d2s", head)
        elif op == "Clip":
            clamp = True
            ids[node.output[0]] = ids[node.input[0]]
        else:
            sys.exit(f"unsupported op {op}")
    assert head is not None

    out = bytearray()
    out += struct.pack("<IIB", MAGIC, VERSION, 1)
    encoded = name.encode()
    out += struct.pack("<H", len(encoded)) + encoded
    out += struct.pack("<BB", 0, 0)  # zero padding, raw input range
    out += struct.pack("<H", len(tensors))
    for channels in tensors:
        out += struct.pack("<H", channels)

    def floats(values):
        return np.asarray(values, dtype=np.float32).astype(np.float16).tobytes()

    out += struct.pack("<H", len(layers))
    for layer in layers:
        out += struct.pack("<H", layer["output"])
        residual = 0xFFFF if layer["residual"] is None else layer["residual"]
        out += struct.pack("<HB", residual, layer["activation"])
        out += struct.pack("<B", len(layer["inputs"]))
        for tensor, transform, kernel, weights in layer["inputs"]:
            out += struct.pack("<HBB", tensor, transform, kernel)
            out += floats(weights)
        out += floats(layer["bias"])
        out += struct.pack("<B", 0)  # no border variants
    out += struct.pack("<HBBB", head, 0, 0, 1 if clamp else 0)
    return bytes(out)


def make_test_input(height=72, width=128):
    """Same deterministic patch as export_artcnn.py."""
    rng = np.random.default_rng(20260611)
    y, x = np.mgrid[0:height, 0:width].astype(np.float32)
    img = 0.35 + 0.3 * np.sin(x / 9.0) * np.cos(y / 7.0)
    img += 0.25 * ((x + y * 1.7) % 23.0 < 2.0)
    img += 0.05 * rng.standard_normal((height, width)).astype(np.float32)
    img = np.clip(img, 0.0, 1.0)
    return (np.round(img * 255.0) / 255.0).astype(np.float32)


def write_test_vector(onnx_path, directory, height, width):
    import onnxruntime as ort

    img = make_test_input(height, width)
    session = ort.InferenceSession(onnx_path, providers=["CPUExecutionProvider"])
    output = session.run(None, {"input": img[None, None]})[0][0, 0]
    with open(f"{directory}/input_{width}x{height}.f32", "wb") as f:
        f.write(img.tobytes())
    with open(f"{directory}/output_{width * 2}x{height * 2}.f32", "wb") as f:
        f.write(output.astype(np.float32).tobytes())


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("onnx_path")
    parser.add_argument("esr_path")
    parser.add_argument("--name")
    parser.add_argument("--test-vector", metavar="DIR")
    parser.add_argument("--test-size", default="128x72")
    args = parser.parse_args()
    model = onnx.load(args.onnx_path)
    name = args.name or args.esr_path.rsplit("/", 1)[-1].removesuffix(".esr")
    blob = convert(model, name)
    with open(args.esr_path, "wb") as f:
        f.write(blob)
    print(f"{args.esr_path}: {len(blob)} bytes")
    if args.test_vector:
        width, height = (int(v) for v in args.test_size.split("x"))
        write_test_vector(args.onnx_path, args.test_vector, height, width)


if __name__ == "__main__":
    main()
