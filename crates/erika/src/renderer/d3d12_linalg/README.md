# D3D12 SM 6.10 linalg inference (preview, opt-in)

This backend executes `sr::catalog::execution_model()` beside the D3D11 renderer.
It is deliberately not wired into `renderer.rs` in this change. There is no
automatic selection and no change to the existing renderer or Vulkan backend.

## Obtain the runtime and compiler

As of 2026-11-02, SM 6.10 is preview. Install a supporting vendor driver and
enable Windows **Settings → System → Advanced → For developers → Developer
Mode** if experimental shader model activation reports that it is required.
The backend and setup script never change system settings.

Use Microsoft's official packages:

* [Agility SDK 1.722-preview](https://www.nuget.org/packages/Microsoft.Direct3D.D3D12/1.722.0-preview), extract the nupkg (ZIP).
* [DXC SM 6.10 Preview, patch 2](https://github.com/microsoft/DirectXShaderCompiler/releases/tag/v1.10.2605.37), extract `dxc_2026_08_11.zip`.

`setup-preview.ps1 -WorkDir C:\path\to\preview-tools` downloads/extracts those
packages and configures the current PowerShell process. Alternatively set:

```powershell
$env:ERIKA_SR_D3D12_LINALG = '1'
$env:ERIKA_D3D12_AGILITY_SDK_PATH = 'C:\preview-tools\agility\build\native\bin\x64'
$env:ERIKA_D3D12_DXCOMPILER_PATH = 'C:\preview-tools\dxc\bin\x64\dxcompiler.dll'
$env:ERIKA_D3D12_DXC_INCLUDE_PATH = 'C:\preview-tools\dxc\inc\hlsl'
# Optional: fp32 (default) or fp16 accumulation. Both have fp16 operands.
$env:ERIKA_SR_D3D12_LINALG_ACCUMULATOR = 'fp32'
# Optional: use the matching version if installing a later preview SDK.
$env:ERIKA_D3D12_AGILITY_SDK_VERSION = '722'
# Optional: validation with the preview SDK's debug layer (slower).
$env:ERIKA_SR_D3D12_LINALG_VALIDATION = '1'
```

The Agility path must be absolute and ASCII because `CreateDeviceFactory`
takes an ANSI path. Keep the matching compiler DLL dependencies (including
`dxil.dll`) beside the configured `dxcompiler.dll`, and use its matching HLSL
headers. Runtime compilation handles arbitrary validated custom graphs and
embeds only Erika's source, so this repository redistributes no Microsoft
binaries. DXC and its header directory must remain available while preparing
models. The explicitly loaded compiler module stays pinned for process lifetime.

## Integration API

Add this module declaration yourself on Windows:

```rust,ignore
#[cfg(target_os = "windows")]
mod d3d11_d3d12_linalg;
```

1. `D3d12LinalgUpscaler::new(&device)` requires the opt-in variable and preview
   SDK. It uses `ID3D12SDKConfiguration1::CreateDeviceFactory`, enables the
   shader-model experiment on that private factory, and creates the D3D12
   device on the original D3D11 adapter, verifying its LUID. It does not call
   `SetSDKVersion` or `ApplyToGlobalState`, or require host EXE exports.
2. Creation checks SM `0x6a`, linear algebra tier 1.0, and **native** wave32
   16×16×16 fp16 multiplication with the selected accumulator. Emulated
   operands/results are rejected. Preview capability structures are local
   `repr(C)` definitions from the 1.722 header because windows-rs 0.62.2
   predates those declarations.
3. `supports(&model)` checks graph validity, a final-layer head, and scratch
   requirements. `prepare(Arc<SrModel>)` compiles a pipeline for each layer and
   stages immutable weights. The first encode copies them to GPU-local memory
   on the same command list before inference. Prepare outside latency-sensitive playback work;
   preview compilation/driver pipeline creation can take seconds.
4. `encode(&device, &immediate_context, &luma_array_srv, w, h, range)` clobbers
   compute state, then unbinds its D3D11 copy shader and resources. Consume the
   returned **Texture2D R16_FLOAT** SRV at **2w × 2h**, on the same immediate
   context, before the next encode. This differs from the legacy ArtCNN packed
   RGBA output: the consumer needs a direct luma sample, not packed-channel
   unpacking. The source is slice zero of the caller's Texture2DArray view.
5. Record downstream reads on that same context. Fence ordering covers them
   before a later encode reuses output storage. A retained SRV is not an
   immutable frame snapshot. Recreate on decoder device changes. Propagate
   errors to the caller's existing fallback policy.
6. `last_gpu_duration()` returns the last completed D3D12 timestamp interval,
   covering all tile dispatches and barriers; initial calls return `None`.
   It excludes the D3D11 copy and cross-queue handoff. The benchmark also uses
   D3D11 timestamps for the complete operation.

There are at most three frame slots. A busy ring produces a descriptive error
instead of blocking the CPU. The caller may retry on another tick or use its
fallback path. There are no explicit CPU waits in steady-state `encode`.
Destruction drains outstanding work before releasing command allocators.

## Computation and synchronization

* A small D3D11 compute shader copies raw plane values to a shared R32F
  texture. This works for NV12/P010 plane SRVs, which cannot be copied to R32F
  with `CopyResource`. The shader performs no range conversion.
* D3D11 signals a shared `ID3D11Fence` and flushes submission. The D3D12 queue
  waits on its shared `ID3D12Fence` view, runs the network, signals completion,
  and the D3D11 immediate context waits before its subsequent luma reads.
* D3D12-created input/output textures use shared heaps and simultaneous access;
  D3D11 opens their NT handles with `OpenSharedResource1`. Render-target
  compatibility is also required for this driver to import the UAV textures.
  Handles are closed immediately after opening the second API view.
* Tiles grow to the frame dimensions within a **256 MiB scratch budget per
  slot**. Tensor slots are reused after their last reader; features use packed
  native fp16 buffers. C4F16/C4F32 1080p normally run as one tile, R8F64 720p
  as two. `tensor_halos()` supplies exact overlaps and shrinking layer regions.
  Input/output storage scales with image size (maximum 8192 input edge).
* Each wave retains several 16-pixel accumulator fragments and computes
  multiple output-channel blocks. Native K16/K32/K64 loads read shared input
  windows without materializing im2col in device memory. Prepared fp16 weights
  use column-major K×OC storage. Small layers cache weights; 32-channel layers
  stream shared K blocks; 64-channel layers load fragments directly from
  GPU-local weights, which measured faster than repeated group barriers.
  Shared leading-dimension padding reduces bank conflicts. CReLU transforms
  apply while loading fragments. Compatible intermediate layers reuse their
  shared input storage for accumulator stores and write packed fp16 vectors.
  The preview validator requires `Align=128` for matrix descriptor loads.
  Packed weights start at 128-byte boundaries; direct loading is restricted
  to column-major shapes whose K steps and input-segment starts preserve that
  alignment. Other shapes use shared loads. Caches cover the whole reduction;
  configurations that cannot fit use the bounded legacy matrix kernel.
  Matrix accumulation defaults to fp32; fp16 is explicitly selectable. The
  four-channel head uses fp32 scalar FMAs on fp16 operands, register-window
  reuse and phased shared storage to avoid underfilled matrix tiles.
* Epilogues fuse bias, residual, and None/ReLU/SiLU/PReLU. Folded layers first
  compute interior weights, then a separate ring dispatch evaluates the eight
  global-border classes with exact parameters before consumers run.
  Padding and border classes use global image coordinates, including the
  reference's first-edge precedence for width/height one.
* The final epilogue fuses depth-to-space (both orders), optional clamped
  bilinear base, inverse input-range mapping, and optional output clamp.
  Tile boundaries do not trigger image-border variants.

## Validation

With the module declaration integrated and preview environment configured:

```powershell
cargo test -p erika --lib d3d11_d3d12_linalg -- --nocapture --test-threads=1
cargo test -p erika --lib --release d3d11_d3d12_linalg::tests::gpu_benchmark -- --ignored --nocapture --test-threads=1
# PIX-free timestamps around every main/ring dispatch; median of five frames.
cargo test -p erika --lib --release d3d11_d3d12_linalg::tests::profile_layers -- --ignored --nocapture --test-threads=1
```

Tests skip clearly if preview creation is unavailable. Once device creation
succeeds, compilation, sharing, execution and numerical failures fail tests.
The compiler-only test validates both accumulators even without a usable GPU.
Numerical tests compare C4F16/C4F32/R8F64/Anime4K M/UL to `reference::run`;
small/narrow images, folded borders, seams, and both luma ranges are covered.
Custom graph tests additionally exercise PReLU, residuals, multiple transformed
inputs, column-major depth-to-space and unclamped output. Negative subprocess
tests verify missing opt-in/SDK failure without changing the parent environment.

The ignored benchmark measures both accumulators at 720p and 1080p, reports
the median of five samples after a warmup, and reports useful TFLOPS as
`2 * macs_per_pixel * width * height / seconds`. It excludes halo/border
overcomputation from its FLOP numerator. Timestamp polling uses non-zero
sentinels because windows-rs maps D3D11 `GetData`'s `S_FALSE` to `Ok`.

Set `ERIKA_SR_D3D12_LINALG_BENCH_MODEL=c4f16,c4f32,r8f64,m,ul` to filter
benchmarks/profiles. `ERIKA_SR_D3D12_LINALG_DUMP_DIR=<absolute-directory>`
dumps generated HLSL and DXIL for inspection. Normal encoding resolves only
the complete-network timestamps; `ERIKA_SR_D3D12_LINALG_PROFILE=1` enables
per-dispatch queries. Profiling does not add CPU waits to `encode()`.

The preview permits only RowMajor/ColMajor layouts for wave matrices.
`MulOptimal`/`OuterProductOptimal` belong to thread-scope matrices and cannot
be used to pre-swizzle this wave implementation. The runtime reports 39,936
shared bytes on the test GPU, but this DXC validates a 32,768-byte limit, so
kernel selection respects 32 KiB. DXIL inspection confirms matrix MMA
intrinsics; it does not establish a particular vendor instruction sequence.

Developer tuning variables use the prefix `ERIKA_SR_D3D12_LINALG_`:

| Suffix | Values / default |
|---|---|
| `K` | 16/32/64 when channels divide evenly; default 64/32/16 by channel shape |
| `FRAGMENTS` | 1/2/4/8; default 4 for ≤16 outputs, otherwise 2; reduced to fit LDS |
| `CHANNEL_BLOCK` | 16/32/64 cap; default up to 64 outputs per group |
| `WEIGHT_CACHE` | 0 disables small-layer caching |
| `STREAM_WEIGHTS` | 0 disables streaming; default enabled for wide spatial layers |
| `DIRECT_WEIGHTS` | 0/1 override; default ≥64 outputs, subject to 128-byte alignment |
| `WEIGHT_LAYOUT` | `row` or default column-major |
| `LDS_PADDING` | 0/4/8/16 half elements; default 8 |
| `SHARED_EPILOGUE` | 0 disables shared accumulator/vector stores; default enabled |
| `SCALAR_HEAD` | 0 selects the matrix head; scalar default |
| `HEAD_WIDTH`, `HEAD_HEIGHT`, `HEAD_QUADS` | Default 32,16,2; channel phases limited by LDS |
| `FIRST_IM2COL` | 1 tests a staged one-channel input matrix; default disabled |

These overrides apply during prepare. Revalidate the oracle after changing
geometry. Native matrix caps are queried for every selected shape; unsupported
or emulated shapes fail with a reason. The standard layout path remains
available for irregular shapes that cannot fit a shared-window configuration.

## Retail transition

SM 6.10 is expected in Agility **1.621** in early December 2026. Preview 722
and retail 621 are separate version sequences; a numeric >=722 check is not
a retail version check. Before enabling a retail build:

* Recheck the final header ABI/enums, use the retail DXC and matching headers,
  and rerun compiler, oracle, debug-layer and performance tests.
* Change the private factory's default SDK version from 722 to 621 and remove
  the preview-only minimum-version guard and experiment activation. Permit
  an absent Agility path only when an installed runtime actually exposes SM
  6.10 and the same native matrix caps; retain all runtime feature checks.
* Keep opt-in until the renderer integration and supported-driver matrix are
  validated, then change the selection policy in the integration layer.

No date-based enablement or assumption of future driver support is present.
