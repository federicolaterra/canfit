"""Write tiny synthetic GGUF files for the canfit smoke test (no real model needed).

usage: python mkgguf.py tiny|huge PATH

`tiny` is a 2-layer llama-style model (~82 KB, needs about 1 GB with canfit's overhead).
`huge` declares ~225 GB of Q4_K weights. The file is extended without writing the data, so it
is sparse on APFS, ext4 and (after `fsutil sparse setflag`) NTFS and costs no disk space.
Same layout as tests/support/gguf_builder.rs (GGUF v3, Q4_K tensors).
"""

import os
import struct
import subprocess
import sys

Q4_K = 12  # ggml type id: 256 elements per 144-byte block


def _s(b: bytes) -> bytes:
    return struct.pack("<Q", len(b)) + b


def _kv_str(k: str, v: str) -> bytes:
    return _s(k.encode()) + struct.pack("<I", 8) + _s(v.encode())


def _kv_u32(k: str, v: int) -> bytes:
    return _s(k.encode()) + struct.pack("<I", 4) + struct.pack("<I", v)


def build(path: str, name: str, layers: int, emb: int, ctx: int, blk_dims: list[int]) -> None:
    kvs = [
        _kv_str("general.architecture", "llama"),
        _kv_str("general.name", name),
        _kv_u32("general.file_type", 15),
        _kv_u32("llama.block_count", layers),
        _kv_u32("llama.context_length", ctx),
        _kv_u32("llama.embedding_length", emb),
        _kv_u32("llama.attention.head_count", 8),
        _kv_u32("llama.attention.head_count_kv", 2),
    ]
    tensors = [("token_embd.weight", [emb, 64])]
    tensors += [(f"blk.{i}.attn_q.weight", blk_dims) for i in range(layers)]

    out = b"GGUF" + struct.pack("<I", 3) + struct.pack("<Q", len(tensors))
    out += struct.pack("<Q", len(kvs)) + b"".join(kvs)
    offset = data_len = 0
    for tname, dims in tensors:
        out += _s(tname.encode()) + struct.pack("<I", len(dims))
        out += b"".join(struct.pack("<Q", d) for d in dims)
        out += struct.pack("<I", Q4_K) + struct.pack("<Q", offset)
        elements = 1
        for d in dims:
            elements *= d
        size = elements // 256 * 144
        data_len = offset + size
        offset = -(-(offset + size) // 32) * 32
    out += b"\0" * (-len(out) % 32)

    with open(path, "wb") as f:
        f.write(out)
    size = len(out) + data_len
    if os.name == "nt":
        # Python's truncate() on Windows (the C runtime's _chsize) writes zeros up to the new size,
        # which for the huge model fills the disk. Mark the file sparse and set its end with fsutil
        # instead: NTFS then stores nothing for the unwritten range.
        subprocess.run(["fsutil", "sparse", "setflag", path], check=True, capture_output=True)
        subprocess.run(["fsutil", "file", "seteof", path, str(size)], check=True, capture_output=True)
    else:
        with open(path, "r+b") as f:
            f.truncate(size)


def main() -> None:
    which, path = sys.argv[1], sys.argv[2]
    if which == "tiny":
        build(path, "tiny", 2, 256, 4096, [256, 256])
    elif which == "huge":
        build(path, "huge", 100, 8192, 32768, [65536, 65536])
    else:
        sys.exit(f"unknown model kind {which!r}")


if __name__ == "__main__":
    main()
