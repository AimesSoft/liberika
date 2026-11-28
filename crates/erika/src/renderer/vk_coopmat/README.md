# Vulkan cooperative-matrix shaders

Regenerate with shaderc/glslc supporting `GL_KHR_cooperative_matrix` (tested
with `/opt/homebrew/bin/glslc`):

```sh
set -e
cd crates/erika/src/renderer/vk_coopmat
for shader in conv resident; do
    glslc --target-env=vulkan1.3 -O "$shader.comp" -o "$shader.spv"
    glslc --target-env=vulkan1.3 -O -DHALF_ACCUM "$shader.comp" -o "${shader}_half.spv"
    glslc --target-env=vulkan1.3 -O -DDIRECT_LOAD "$shader.comp" -o "${shader}_direct.spv"
    glslc --target-env=vulkan1.3 -O -DDIRECT_LOAD -DHALF_ACCUM "$shader.comp" -o "${shader}_direct_half.spv"
    glslc --target-env=vulkan1.3 -O -DDIRECT_LOAD -DELEMENT_EPILOGUE "$shader.comp" -o "${shader}_elements.spv"
    glslc --target-env=vulkan1.3 -O -DDIRECT_LOAD -DELEMENT_EPILOGUE -DHALF_ACCUM "$shader.comp" -o "${shader}_elements_half.spv"
done
for shader in input border border_specialized pad head head_point head_spatial head_spatial_half peak fused bandwidth; do
    glslc --target-env=vulkan1.3 -O "$shader.comp" -o "$shader.spv"
done
glslc --target-env=vulkan1.3 -O -DHALF_ACCUM peak.comp -o peak_half.spv
glslc --target-env=vulkan1.3 -O -DHALF_ACCUM fused.comp -o fused_half.spv
```

`copy.hlsl` is the D3D11 luma copy, compiled with FXC at initialization.
Other shaders compile ahead of time; runtime shader compilation is unnecessary.
Keep regenerated SPIR-V with the sources.

Features are fp16 `[channel / 16][pixel][channel % 16]`; tensor bases align
to at least 16 channels. Weights include padded per-tap output-by-input
matrices and an OIHW copy for exact border correction. The resident kernel
keeps A fragments in subgroup registers and reuses them across pixel strips.
For K=16, column-major B loads come directly from feature buffers. Other K
sizes retain shared staging with pitch K+8. Luma-only inputs flatten spatial
taps into K, including the luma branch of a folded head.

The element epilogue variant loads a coordinate matrix with exactly the
accumulator's shape and type. Element-wise coordinate access identifies
each implementation-defined accumulator component without assuming a
vendor's register layout. It writes results directly, avoiding shared sums
and their barrier. This is selected for graphs up to 32 channels. Wider
graphs retain the previous kernel choices. Both variants fuse bias,
residual and activation. Heads fuse depth-to-space, base reconstruction,
normalization inversion and clamp. Small heads use vector ALUs with a
16 KiB coefficient buffer and specialized descriptors; 1x1 heads avoid
shared staging entirely. NVIDIA defaults to the packed fp16 spatial head for
the exact bundled C4F16-DN/C4F32-DN graphs. It loads contiguous input channels
into a padded fp16 shared tile and uses four-channel fp16 FMA before the fp32
epilogue. Each coefficient quartet retains its 16-byte slot, with four halves
packed into the first two words. Selection compares the complete graph and
weights, so a custom model's name alone does not change its precision policy.
Other vector heads use fp32 coefficients/accumulators. Larger heads use
matrices. Folded border classes
are corrected before consumers; narrow graphs use specialized OIHW loops.

Binding 0: feature arena; 1: fp16 weights; 2: u32 metadata (all storage
buffers); 3: raw R32F input; 4: raw-domain R16F output (storage images);
5: head coefficients (uniform buffer). Element epilogues also interpret
the coordinate-table portion of binding 2 as fp32. `pack_model` defines the
metadata and coordinate tables; offsets count elements. Push constants are
five 16-byte vectors: image/arena size, origins, core/header, normalization,
and active region. Regions shrink with tensor halos. Right/bottom guards
make partial matrix loads safe.

Common specialization IDs: 0..3 = M/N/K/subgroup width; 4 = maximum kernel;
5..20 = layer header; 21..25 = common input information; 26 = subgroups;
27 = output blocks; 28 = luma stem; 29 = actual input channels; 30 = strips;
31 = single input. ID32 gives the coordinate-table offset for element
epilogues. Specialized heads instead use IDs32..95 for up to 16 input
descriptors (feature base, transform, actual channels, coefficient base).
The experimental fused shader defines its own IDs32..50 in `fused.comp`.

Fp16 operands/features are used with both matrix accumulation variants. Default
matrix accumulation remains fp32. `ERIKA_VK_FP16_ACCUMULATION=1` selects fp16 when
supported. Tuning flags below are read during construction/prepare and
are diagnostics, not additional renderer-facing API:

- `ERIKA_VK_DIRECT_LOAD=0`: shared B staging.
- `ERIKA_VK_ELEMENTS=0`: shared accumulator epilogues.
- `ERIKA_VK_RESIDENT=0`: stream A fragments.
- `ERIKA_VK_EDGE_GROUPS=1|2|4|8`: override stem/matrix-head subgroup count.
- `ERIKA_VK_TENSOR_HEAD=1`: force matrix heads; `0`: request vector heads
  where coefficient/shared-memory limits permit.
- `ERIKA_VK_POINT_HEAD=0`, `ERIKA_VK_SPATIAL_HEAD=0`: old generic vector head.
- `ERIKA_VK_FP16_HEAD=0`: retain the fp32 spatial head. `1` also permits the
  validated packed-head path on other vendors; graph/capability checks still
  apply. This is independent of matrix accumulation precision.
- `ERIKA_VK_BORDER=0`: old scalar ring loop; `1`: specialized loop for all
  widths (default specializes only graphs up to 32 channels).
- `ERIKA_VK_BAND_MIB=N`: horizontal bands bounded by the requested working
  set where the minimum halo fits. Default uses the measured 240 MiB policy.
- `ERIKA_VK_FUSION=2|3`: experimental shared convolution chains.
  `ERIKA_VK_FUSE_STEM=1` also includes an eligible 5x5 stem; fusion value `1`
  then tests just stem + first 3x3 layer. `ERIKA_VK_FUSION_TILE=32x8` and
  `ERIKA_VK_FUSION_GROUPS=1|2|4|8` tune it. Fusion is disabled by default:
  measured halo/register/shared-memory costs outweighed traffic savings.

Fusion validates unbranched same-width chains, preserves outside consumers,
and adjusts arena liveness so its final output never aliases a live input.
The fused stem handles its exact folded border weights inside the workgroup.
Intermediate fp16 tensors remain in shared memory; Clamp padding is applied
between stages. No experimental path is selected for R8F64 by default.

Ignored Rust tests provide end-to-end GPU benchmarks, per-layer convolution
and correction timestamps, memory bandwidth, matrix throughput, and
tile/strip sweeps. Run release with one test thread. See the **Round 3**
section of `/tmp/sr/codex/vk_report.md` for results and limitations.
