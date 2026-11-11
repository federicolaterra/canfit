# Memory estimation for GGUF models

Apps are estimated differently, from the memory their settings declare: see [Apps](apps.md).

`canfit model.gguf` reads the model's GGUF header — never the weights themselves — and estimates
how much memory llama.cpp needs to run it. The result is always presented as an **estimate**, and
each part says where it comes from.

## Formula

```text
total ≈ weights + KV cache + overhead + margin

weights    = sum of every tensor's size (exact, read from the GGUF file)
KV cache   = context × Σ over layers ( n_head_kv × (key_length + value_length) ) × bytes_per_element
             = 2 × n_layer × n_ctx × n_head_kv × head_dim × bytes_per_element when every layer is alike
head_dim   = n_embd / n_head when the file does not state key/value lengths
overhead   = compute buffers + runtime (see below)
margin     = 10 % of the rest by default (--margin)
```

Bytes per KV element follow llama.cpp's cache types: `f16` = 2, `q8_0` = 34/32 ≈ 1.06, `q4_0` =
18/32 ≈ 0.56 (`--cache-type`, default `f16`).

The KV cache formula handles:

- **Grouped-query attention**: `n_head_kv` smaller than `n_head`.
- **Per-layer head counts**: hybrid models (for example LFM2) store `head_count_kv` per layer;
  layers with zero KV heads (recurrent or convolution layers) need no KV cache.
- **Explicit key/value lengths** (`attention.key_length`, `attention.value_length`).
- **DeepSeek-style latent attention (MLA)**: when `attention.kv_lora_rank` is present, the cache
  holds one latent of `kv_lora_rank + rope.dimension_count` elements per layer and token.
- **Mixture-of-experts models**: every expert's weights count, because all of them stay in memory
  even though few are active per token.

### Context length

`--ctx N` sets the context. Without it canfit uses **8,192 tokens**, or the model's own context
length when that is smaller. llama.cpp would load the model's full context by default and then
shrink it to fit (`--fit`); Ollama and LM Studio default to a few thousand tokens. 8,192 covers
those defaults without turning every 128k-context model into "doesn't fit". The card always says
which context it used.

### Overhead

Compute buffers and the runtime grow with the context and the model:

```text
overhead = fixed + n_ctx × n_embd × per_token + share × weights
```

| Backend | fixed | per_token | share | Status |
|---|---|---|---|---|
| llama.cpp + Metal (Apple Silicon) | 256 MiB | 2 bytes | 2 % | calibrated against llama.cpp's own buffer accounting |
| everything else (CPU, CUDA, Vulkan) | 1 GiB | — | 7.6 % | starting heuristic, deliberately high until measured |

On Apple Silicon the calibrated profile lands a few percent **above** what llama.cpp actually
allocates; the margin comes on top. Other backends keep the conservative starting heuristic until
they are measured on real machines.

### Example

A model with 32 layers, 8 KV heads, a head dimension of 128 and an f16 cache at 8,192 tokens of
context needs `2 × 32 × 8192 × 8 × 128 × 2 bytes = 1 GiB` of KV cache, on top of its weights.

## Value labels

| Part | Label |
|---|---|
| Weights, layer and head counts | read from the GGUF file (exact) — `read_from_config` in JSON |
| KV cache | estimated (exact for llama.cpp's allocation in our tests; sliding-window models run high) |
| Overhead and margin | estimated |
| Memory available now | measured |
| GPU memory limit | read from config when you set `iogpu.wired_limit_mb`, otherwise estimated |

## The verdict

canfit compares the estimate with the memory available **now** — free memory plus file cache the
system can reclaim, as Activity Monitor counts it — and, on Apple Silicon, with the share of
memory the GPU may use.

| Result | Exit code | When |
|---|---|---|
| **Fits** | `0` | The estimate including the margin fits in available memory (and in the GPU limit) |
| **Tight** | `1` | It fits only without the safety margin, so the system may compress or swap; or it fits but memory pressure is already **critical** |
| **Doesn't fit** | `2` | Even without the margin it needs more than is available, or more than the GPU may use |

On Apple Silicon both limits are checked and the result is the worse of the two; the card names
the limit that decided it. canfit assumes llama.cpp puts every layer on the GPU, its default on
Apple Silicon. The GPU limit is a hard ceiling on what the GPU may wire, not on what is free now:
other GPU users (the window server, other models) share it.

Several models on one command line are summed, each with its own overhead, because each runs in
its own process.

## Files canfit accepts

- A `.gguf` file, or any file that starts with the GGUF signature (for example an Ollama blob).
- A split model: pass any part (`name-00001-of-00003.gguf`) and canfit reads all of them; every
  part must be in the same folder.
- A folder that holds exactly one model (one file or one split set).

GGUF versions 2 and 3 are supported. canfit reads only the header and checks every count against
the file size, so a broken or partly downloaded file gets a clear error (exit code `3`) instead of
a wrong answer.

## Checking an estimate on your machine

`canfit watch -- llama-server -m model.gguf -c 8192` runs the server and, when it ends, puts the
measured peak of its whole process tree next to canfit's estimate for that model and context
([Watch](usage.md#watch)). llama.cpp maps the weights from the file by default; macOS's
physical footprint and Windows's private working set leave mapped pages out, so canfit adds the
weights (read from the file) before comparing. On the Mac, a Qwen2.5 Coder 3B (Q4_K_M) at 8,192
tokens measured 381 MB + 1.8 GB mapped (9 % below the 2.4 GB estimate without margin), and 2.7 GB
with the weights loaded into memory (`--load-mode none`), 11 % above it at the load peak and on
the estimate once loaded.

## Known limits

- **Sliding-window attention** (Gemma 2/3 and others): llama.cpp keeps a smaller cache for the
  sliding-window layers; canfit sizes a full cache for every layer, so estimates at long contexts
  run high. The card says so.
- **Recurrent state** (Mamba, RWKV, LFM2 convolution layers) is small and not counted.
- **Partial offload** (`--n-gpu-layers` lower than the layer count) is not modelled yet.
- **Models already loaded** by llama.cpp, Ollama or LM Studio are not detected yet: their memory
  is simply not available.
- Calibration so far covers llama.cpp with Metal on one Apple Silicon Mac.
