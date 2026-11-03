//! Optional SM 6.10 preview inference beside the D3D11 renderer.
//! See d3d12_linalg/README.md for runtime installation and integration.
#![cfg(target_os = "windows")]

use super::sr::{
    model::*,
    reference::{LumaRange, input_normalization},
};
use crate::core::{PlayerError, Result};
use std::{
    ffi::{CString, c_void},
    mem::{self, ManuallyDrop},
    path::PathBuf,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, HMODULE},
        Graphics::{
            Direct3D::Dxc::*, Direct3D::*, Direct3D11::*, Direct3D12::*, Dxgi::Common::*, Dxgi::*,
        },
        System::LibraryLoader::{
            GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
            LoadLibraryExW,
        },
    },
    core::{GUID, Interface, PCSTR, PCWSTR},
};

const CORE_W: u32 = 128;
const CORE_H: u32 = 64;
const MAX_SLOTS: usize = 3;
const MAX_SCRATCH: u64 = 256 * 1024 * 1024;
fn fail(message: impl Into<String>) -> PlayerError {
    PlayerError::Renderer(format!("D3D12 linalg: {}", message.into()))
}
fn win<T>(r: windows::core::Result<T>, what: &str) -> Result<T> {
    r.map_err(|e| fail(format!("{what}: {e}")))
}
fn some<T>(v: Option<T>, what: &str) -> Result<T> {
    v.ok_or_else(|| fail(format!("{what} returned null")))
}
fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain(Some(0)).collect()
}

// windows 0.62.2 predates these preview-only ABI declarations. Values and
// layouts come from Microsoft.Direct3D.D3D12 1.722.0-preview d3d12.h.
#[repr(C)]
#[derive(Default)]
struct MatrixCaps {
    operation: u32,
    data: [u32; 11],
}
fn matrix_support(device: &ID3D12Device, half: bool) -> Result<()> {
    let mut tier = 0u32;
    unsafe {
        win(
            device.CheckFeatureSupport(D3D12_FEATURE(77), &mut tier as *mut _ as _, 4),
            "LINEAR_ALGEBRA_SUPPORT (77)",
        )?;
    }
    if tier < 0x10 {
        return Err(fail(format!(
            "linear algebra tier {tier:#x}; tier 1.0 required"
        )));
    }
    // Union is sized for the largest member (ThreadGroupMatrixMultiply).
    let mut caps = MatrixCaps {
        operation: 1,
        ..Default::default()
    };
    caps.data[..7].copy_from_slice(&[32, 7, 7, if half { 7 } else { 8 }, 16, 16, 16]);
    unsafe {
        win(
            device.CheckFeatureSupport(
                D3D12_FEATURE(78),
                &mut caps as *mut _ as _,
                mem::size_of::<MatrixCaps>() as u32,
            ),
            "WAVE_MATRIX_MULTIPLY (78)",
        )?;
    }
    if caps.data[7] & 1 == 0 || caps.data[7] & 6 != 0 {
        return Err(fail(format!(
            "native 16x16x16 fp16 wave32 multiply unsupported (flags {:#x}, accumulator {})",
            caps.data[7],
            if half { "fp16" } else { "fp32" }
        )));
    }
    Ok(())
}

struct Dxc {
    compiler: IDxcCompiler3,
    include: IDxcIncludeHandler,
    headers: PathBuf,
    _module: HMODULE,
}
impl Dxc {
    fn new() -> Result<Self> {
        let path = PathBuf::from(std::env::var_os("ERIKA_D3D12_DXCOMPILER_PATH").ok_or_else(
            || {
                fail(
                    "set ERIKA_D3D12_DXCOMPILER_PATH to the preview dxcompiler.dll (absolute path)",
                )
            },
        )?);
        if !path.is_absolute() {
            return Err(fail("DXCOMPILER_PATH must be absolute"));
        }
        let name = wide(path.as_os_str());
        unsafe {
            let module = win(
                LoadLibraryExW(
                    PCWSTR(name.as_ptr()),
                    None,
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
                ),
                "load configured DXC",
            )?;
            let address = GetProcAddress(module, windows::core::s!("DxcCreateInstance"))
                .ok_or_else(|| fail("DXC has no DxcCreateInstance"))?;
            let create: unsafe extern "system" fn(
                *const GUID,
                *const GUID,
                *mut *mut c_void,
            ) -> windows::core::HRESULT = mem::transmute(address);
            let mut compiler = ptr::null_mut();
            win(
                create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut compiler).ok(),
                "create DXC compiler",
            )?;
            let compiler = IDxcCompiler3::from_raw(compiler);
            let mut utils = ptr::null_mut();
            win(
                create(&CLSID_DxcUtils, &IDxcUtils::IID, &mut utils).ok(),
                "create DXC utilities",
            )?;
            let utils = IDxcUtils::from_raw(utils);
            let include = win(utils.CreateDefaultIncludeHandler(), "DXC include handler")?;
            let headers = PathBuf::from(std::env::var_os("ERIKA_D3D12_DXC_INCLUDE_PATH").ok_or_else(|| fail("set ERIKA_D3D12_DXC_INCLUDE_PATH to the preview include/hlsl directory containing dx/linalg.h"))?);
            if !headers.join("dx/linalg.h").is_file() {
                return Err(fail("DXC include directory does not contain dx/linalg.h"));
            }
            // Deliberately pin the explicitly loaded module for process lifetime:
            // DXC worker threads and COM vtables can outlive an individual model.
            Ok(Self {
                compiler,
                include,
                headers,
                _module: module,
            })
        }
    }
    fn compile(&self, source: &str, half: bool) -> Result<Vec<u8>> {
        let mut args = vec![
            "-T".to_string(),
            "cs_6_10".into(),
            "-E".into(),
            "main".into(),
            "-HV".into(),
            "2021".into(),
            "-enable-16bit-types".into(),
            "-O3".into(),
            "-I".into(),
            self.headers.to_string_lossy().into_owned(),
        ];
        if half {
            args.push("-DACCUMULATE_HALF=1".into());
        }
        let strings: Vec<_> = args.iter().map(|s| wide(s.as_ref())).collect();
        let pointers: Vec<_> = strings.iter().map(|s| PCWSTR(s.as_ptr())).collect();
        let buffer = DxcBuffer {
            Ptr: source.as_ptr() as _,
            Size: source.len(),
            Encoding: DXC_CP_UTF8.0,
        };
        unsafe {
            let result: IDxcResult = win(
                self.compiler
                    .Compile(&buffer, Some(&pointers), &self.include),
                "DXC compile",
            )?;
            let status = win(result.GetStatus(), "DXC status")?;
            if status.is_err() {
                let errors = win(result.GetErrorBuffer(), "DXC errors")?;
                let bytes = std::slice::from_raw_parts(
                    errors.GetBufferPointer() as *const u8,
                    errors.GetBufferSize(),
                );
                return Err(fail(format!(
                    "cs_6_10 compilation: {}",
                    String::from_utf8_lossy(bytes)
                )));
            }
            let blob = win(result.GetResult(), "DXC object")?;
            Ok(std::slice::from_raw_parts(
                blob.GetBufferPointer() as *const u8,
                blob.GetBufferSize(),
            )
            .to_vec())
        }
    }
}

pub(crate) struct D3d12LinalgUpscaler {
    device: ID3D12Device,
    source_device: ID3D11Device,
    _factory: ID3D12DeviceFactory,
    queue: ID3D12CommandQueue,
    fence12: ID3D12Fence,
    fence11: ID3D11Fence,
    timeline: u64,
    root: ID3D12RootSignature,
    dxc: Dxc,
    half: bool,
    prepared: Option<Arc<Prepared>>,
    slots: Vec<Slot>,
    duration: Option<Duration>,
    context: Option<ID3D11DeviceContext4>,
    profile: bool,
    last_layers: Vec<(Duration, Duration, usize)>,
    last_dispatches: Vec<(usize, bool, Duration)>,
    tensor_budget: u64,
}
impl D3d12LinalgUpscaler {
    /// Requires explicit opt-in, preview Agility and a matching preview DXC.
    pub(crate) fn new(device: &ID3D11Device) -> Result<Self> {
        if std::env::var("ERIKA_SR_D3D12_LINALG").as_deref() != Ok("1") {
            return Err(fail("disabled; set ERIKA_SR_D3D12_LINALG=1 to opt in"));
        }
        let path = PathBuf::from(std::env::var_os("ERIKA_D3D12_AGILITY_SDK_PATH").ok_or_else(|| fail("preview requires ERIKA_D3D12_AGILITY_SDK_PATH containing Agility 1.722-preview D3D12Core.dll"))?);
        if !path.is_absolute() || !path.join("D3D12Core.dll").is_file() {
            return Err(fail(
                "AGILITY_SDK_PATH must be an absolute directory containing D3D12Core.dll",
            ));
        }
        if !path.as_os_str().as_encoded_bytes().is_ascii() {
            return Err(fail(
                "Agility SDK path must be ASCII (CreateDeviceFactory takes an ANSI path)",
            ));
        }
        let sdk_version =
            std::env::var("ERIKA_D3D12_AGILITY_SDK_VERSION").map_or(Ok(722), |s| {
                s.parse::<u32>()
                    .map_err(|_| fail("invalid AGILITY_SDK_VERSION"))
            })?;
        if sdk_version < 722 || sdk_version >= 1000 {
            return Err(fail(
                "preview SDK version must be 722 or newer (for example 722 for 1.722-preview)",
            ));
        }
        let path = CString::new(format!(
            "{}\\",
            path.to_string_lossy().trim_end_matches(['/', '\\'])
        ))
        .map_err(|_| fail("SDK path contains NUL"))?;
        let half = match std::env::var("ERIKA_SR_D3D12_LINALG_ACCUMULATOR").as_deref() {
            Ok("fp16") => true,
            Ok("fp32") | Err(_) => false,
            _ => return Err(fail("ACCUMULATOR must be fp32 or fp16")),
        };
        unsafe {
            let dxgi: IDXGIDevice = win(device.cast(), "D3D11 DXGI device")?;
            let adapter = win(dxgi.GetAdapter(), "D3D11 adapter")?;
            let luid = win(adapter.GetDesc(), "adapter description")?.AdapterLuid;
            let mut config: Option<ID3D12SDKConfiguration1> = None;
            win(
                D3D12GetInterface(&CLSID_D3D12SDKConfiguration, &mut config),
                "ID3D12SDKConfiguration1",
            )?;
            let factory: ID3D12DeviceFactory = win(
                some(config, "SDK configuration")?
                    .CreateDeviceFactory(sdk_version, PCSTR(path.as_ptr() as _)),
                "CreateDeviceFactory(722); install Agility 1.722-preview",
            )?;
            win(
                factory.EnableExperimentalFeatures(1, &D3D12ExperimentalShaderModels, None, None),
                "enable experimental shader models; Windows Developer Mode is required for preview",
            )?;
            if std::env::var("ERIKA_SR_D3D12_LINALG_VALIDATION").as_deref() == Ok("1") {
                let debug: ID3D12Debug = win(
                    factory.GetConfigurationInterface(&CLSID_D3D12Debug),
                    "preview debug interface",
                )?;
                debug.EnableDebugLayer();
            }
            let mut d12: Option<ID3D12Device> = None;
            win(
                factory.CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut d12),
                "create D3D12 device on D3D11 adapter",
            )?;
            let d12 = some(d12, "D3D12 device")?;
            let actual = d12.GetAdapterLuid();
            if actual.HighPart != luid.HighPart || actual.LowPart != luid.LowPart {
                return Err(fail("D3D11/D3D12 adapter LUID mismatch"));
            }
            let mut shader = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL(0x6a),
            };
            win(
                d12.CheckFeatureSupport(
                    D3D12_FEATURE_SHADER_MODEL,
                    &mut shader as *mut _ as _,
                    mem::size_of_val(&shader) as u32,
                ),
                "query SM 6.10",
            )?;
            if shader.HighestShaderModel.0 < 0x6a {
                return Err(fail(format!(
                    "adapter exposes shader model {:#x}, requires 0x6a (6.10)",
                    shader.HighestShaderModel.0
                )));
            }
            matrix_support(&d12, half)?;
            let dev5: ID3D11Device5 = win(device.cast(), "D3D11Device5 shared fences")?;
            let mut fence11: Option<ID3D11Fence> = None;
            win(
                dev5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence11),
                "create D3D11 shared fence",
            )?;
            let fence11 = some(fence11, "D3D11 fence")?;
            let handle = OwnedHandle(win(
                fence11.CreateSharedHandle(None, GENERIC_ALL_ACCESS, PCWSTR::null()),
                "share fence",
            )?);
            let mut fence12 = None;
            win(
                d12.OpenSharedHandle(handle.0, &mut fence12),
                "open fence in D3D12",
            )?;
            let queue = win(
                d12.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                }),
                "create compute queue",
            )?;
            let root = create_root(&d12)?;
            Ok(Self {
                device: d12,
                source_device: device.clone(),
                _factory: factory,
                queue,
                fence12: some(fence12, "D3D12 fence")?,
                fence11,
                timeline: 0,
                root,
                dxc: Dxc::new()?,
                half,
                prepared: None,
                slots: Vec::new(),
                duration: None,
                context: None,
                profile: std::env::var("ERIKA_SR_D3D12_LINALG_PROFILE").as_deref() == Ok("1"),
                last_layers: Vec::new(),
                last_dispatches: Vec::new(),
                tensor_budget: MAX_SCRATCH,
            })
        }
    }
    pub(crate) fn supports(&self, model: &SrModel) -> bool {
        layout(model).is_ok()
    }
    pub(crate) fn prepare(&mut self, model: Arc<SrModel>) -> Result<()> {
        let mut layout = layout(&model)?;
        let mut options = [0u32; 5];
        if unsafe {
            self.device
                .CheckFeatureSupport(D3D12_FEATURE(72), options.as_mut_ptr() as _, 20)
        }
        .is_ok()
        {
            // The preview validator still enforces 32 KiB despite this
            // runtime exposing a larger groupshared allocation.
            layout.shared_budget = options[0].min(32768);
        }
        let mut small = MatrixCaps {
            operation: 1,
            ..Default::default()
        };
        small.data[..7].copy_from_slice(&[32, 7, 7, if self.half { 7 } else { 8 }, 16, 16, 8]);
        let n8 = unsafe {
            self.device.CheckFeatureSupport(
                D3D12_FEATURE(78),
                &mut small as *mut _ as _,
                mem::size_of::<MatrixCaps>() as u32,
            )
        }
        .is_ok()
            && small.data[7] & 1 != 0
            && small.data[7] & 6 == 0;
        layout = layout_for(&model, layout.shared_budget, n8)?;
        let mut checked = Vec::new();
        for (layer, g) in model.layers.iter().zip(&layout.geometry) {
            if g.scalar_head || g.legacy {
                continue;
            }
            let reductions: Vec<_> = if layer.inputs.iter().all(|i| i.kernel == 1) {
                vec![
                    layer
                        .inputs
                        .iter()
                        .map(|i| model.tensors[i.tensor])
                        .sum::<usize>()
                        .div_ceil(16)
                        * 16,
                ]
            } else {
                layer
                    .inputs
                    .iter()
                    .map(|i| model.tensors[i.tensor])
                    .collect()
            };
            for channels in reductions {
                let k = matrix_k(channels);
                if checked.contains(&(k, g.n)) {
                    continue;
                }
                let mut caps = MatrixCaps {
                    operation: 1,
                    ..Default::default()
                };
                caps.data[..7].copy_from_slice(&[
                    32,
                    7,
                    7,
                    if self.half { 7 } else { 8 },
                    16,
                    k as u32,
                    g.n as u32,
                ]);
                unsafe {
                    win(
                        self.device.CheckFeatureSupport(
                            D3D12_FEATURE(78),
                            &mut caps as *mut _ as _,
                            mem::size_of::<MatrixCaps>() as u32,
                        ),
                        "query selected matrix shape",
                    )?;
                }
                if caps.data[7] & 1 == 0 || caps.data[7] & 6 != 0 {
                    return Err(fail(format!(
                        "native wave32 M16 K{k} N{} unsupported (flags {:#x})",
                        g.n, caps.data[7]
                    )));
                }
                checked.push((k, g.n));
            }
        }
        let mut weights = Vec::<f32>::new();
        let mut pipelines = Vec::new();
        let mut borders = Vec::new();
        let mut head_cbvs = Vec::new();
        for layer in &model.layers {
            let source = shader_source(&model, layer, &layout, &mut weights);
            head_cbvs.push(
                source
                    .lines()
                    .find_map(|l| l.strip_prefix("#define HEAD_CBV_OFFSET "))
                    .map(|s| s.parse::<u64>().unwrap()),
            );
            let code = self.dxc.compile(&source, self.half)?;
            if let Some(path) = std::env::var_os("ERIKA_SR_D3D12_LINALG_DUMP_DIR") {
                let path = PathBuf::from(path);
                std::fs::create_dir_all(&path).map_err(|e| fail(e.to_string()))?;
                let name = format!("{}_{}", model.name, layer.output);
                std::fs::write(path.join(format!("{name}.hlsl")), &source)
                    .map_err(|e| fail(e.to_string()))?;
                std::fs::write(path.join(format!("{name}.dxil")), &code)
                    .map_err(|e| fail(e.to_string()))?;
            }
            pipelines.push(compute_pipeline(
                &self.device,
                &self.root,
                &code,
                &format!(
                    "create linalg pipeline {} tensor {} {:?}",
                    model.name,
                    layer.output,
                    layout.geometry[model
                        .layers
                        .iter()
                        .position(|l| std::ptr::eq(l, layer))
                        .unwrap()]
                ),
            )?);
            let index = model
                .layers
                .iter()
                .position(|l| std::ptr::eq(l, layer))
                .unwrap();
            let border = if layer.border.is_some() && !layout.geometry[index].legacy {
                let prefix = source.split_once("// ERIKA_KERNEL_BEGIN").unwrap().0;
                let code = self.dxc.compile(
                    &format!("{prefix}\n{}", include_str!("d3d12_linalg/border.hlsl")),
                    false,
                )?;
                Some(compute_pipeline(
                    &self.device,
                    &self.root,
                    &code,
                    "create exact border pipeline",
                )?)
            } else {
                None
            };
            borders.push(border);
        }
        let upload = buffer(
            &self.device,
            (weights.len() * 4).max(4) as u64,
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_RESOURCE_FLAG_NONE,
        )?;
        unsafe {
            let mut mapped = ptr::null_mut();
            win(
                upload.Map(0, Some(&D3D12_RANGE::default()), Some(&mut mapped)),
                "map weights",
            )?;
            ptr::copy_nonoverlapping(
                weights.as_ptr() as *const u8,
                mapped as *mut u8,
                weights.len() * 4,
            );
            upload.Unmap(0, None);
        }
        let local_weights = buffer(
            &self.device,
            (weights.len() * 4).max(4) as u64,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_NONE,
        )?;
        self.prepared = Some(Arc::new(Prepared {
            model,
            layout,
            weights: local_weights,
            upload,
            uploaded: AtomicBool::new(false),
            pipelines,
            borders,
            head_cbvs,
        }));
        Ok(())
    }
    pub(crate) fn last_gpu_duration(&self) -> Option<Duration> {
        self.duration
    }
}

const GENERIC_ALL_ACCESS: u32 = 0x10000000;
struct OwnedHandle(HANDLE);
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Prepared {
    model: Arc<SrModel>,
    layout: Layout,
    weights: ID3D12Resource,
    upload: ID3D12Resource,
    uploaded: AtomicBool,
    pipelines: Vec<ID3D12PipelineState>,
    borders: Vec<Option<ID3D12PipelineState>>,
    head_cbvs: Vec<Option<u64>>,
}
struct Layout {
    halo: u32,
    offsets: Vec<u32>,
    halos: Vec<u32>,
    pitches: Vec<u32>,
    slot_channels: u32,
    bytes: u64,
    shared_budget: u32,
    geometry: Vec<Geometry>,
}
#[derive(Clone, Copy, Debug)]
struct Geometry {
    n: usize,
    block: usize,
    waves: usize,
    fragments: usize,
    legacy: bool,
    shared_weights: bool,
    col_major: bool,
    cached_k: usize,
    stream_weights: bool,
    direct_weights: bool,
    scalar_head: bool,
    shared_epilogue: bool,
    first_im2col: bool,
    head_width: usize,
}
impl Geometry {
    fn width(self) -> usize {
        if self.scalar_head {
            self.head_width
        } else {
            16
        }
    }
    fn height(self) -> usize {
        self.waves * self.fragments
    }
}
fn sources(model: &SrModel, layer: &ConvLayer) -> Vec<(usize, usize)> {
    let mut src: Vec<(usize, usize)> = Vec::new();
    for i in &layer.inputs {
        if let Some(s) = src.iter_mut().find(|s| s.0 == i.tensor) {
            s.1 = s.1.max(i.kernel / 2);
        } else {
            src.push((i.tensor, i.kernel / 2));
        }
    }
    let _ = model;
    src
}
fn source_pitch(channels: usize) -> usize {
    let padding = std::env::var("ERIKA_SR_D3D12_LINALG_LDS_PADDING")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|p| matches!(p, 0 | 4 | 8 | 16))
        .unwrap_or(8);
    channels + if channels % 16 == 0 { padding } else { 0 }
}
fn matrix_k(channels: usize) -> usize {
    if let Ok(k) = std::env::var("ERIKA_SR_D3D12_LINALG_K") {
        if let Ok(k @ (16 | 32 | 64)) = k.parse::<usize>() {
            if channels % k == 0 {
                return k;
            }
        }
    }
    if channels % 64 == 0 {
        64
    } else if channels % 32 == 0 {
        32
    } else {
        16
    }
}
fn geometry(model: &SrModel, layer: &ConvLayer, budget: u32, n8: bool) -> Geometry {
    let first = layer.inputs.len() == 1 && model.tensors[layer.inputs[0].tensor] == 1;
    let streaming = layer.out_channels >= 32
        && !first
        && std::env::var("ERIKA_SR_D3D12_LINALG_STREAM_WEIGHTS").as_deref() != Ok("0");
    let mut g = Geometry {
        n: if layer.out_channels <= 8 && n8 { 8 } else { 16 },
        block: if streaming {
            layer.out_channels.min(64).div_ceil(16) * 16
        } else if layer.out_channels <= 32 || (first && layer.out_channels <= 64) {
            layer.out_channels.div_ceil(16) * 16
        } else {
            16
        },
        waves: 4,
        fragments: if layer.out_channels <= 16 { 4 } else { 2 },
        legacy: false,
        shared_weights: !streaming
            && std::env::var("ERIKA_SR_D3D12_LINALG_WEIGHT_CACHE").as_deref() != Ok("0"),
        col_major: std::env::var("ERIKA_SR_D3D12_LINALG_WEIGHT_LAYOUT").as_deref() != Ok("row"),
        cached_k: 0,
        stream_weights: streaming,
        direct_weights: false,
        scalar_head: false,
        head_width: std::env::var("ERIKA_SR_D3D12_LINALG_HEAD_WIDTH")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|w| matches!(w, 16 | 32))
            .unwrap_or(32),
        first_im2col: layer.output != model.head.tensor
            && layer.inputs.len() == 1
            && model.tensors[layer.inputs[0].tensor] == 1
            && layer.inputs[0].kernel > 1
            && std::env::var("ERIKA_SR_D3D12_LINALG_FIRST_IM2COL").as_deref() == Ok("1"),
        shared_epilogue: layer.out_channels % 4 == 0
            && !layer.inputs.iter().all(|i| i.kernel == 1)
            && layer
                .inputs
                .iter()
                .any(|i| model.tensors[i.tensor] % 16 == 0)
            && std::env::var("ERIKA_SR_D3D12_LINALG_SHARED_EPILOGUE").as_deref() != Ok("0"),
    };
    g.shared_epilogue |= g.first_im2col
        && layer.out_channels % 4 == 0
        && std::env::var("ERIKA_SR_D3D12_LINALG_SHARED_EPILOGUE").as_deref() != Ok("0");
    if let Ok(fragments) = std::env::var("ERIKA_SR_D3D12_LINALG_FRAGMENTS") {
        if let Ok(fragments @ (1 | 2 | 4 | 8)) = fragments.parse::<usize>() {
            g.fragments = fragments;
        }
    }
    if let Ok(block) = std::env::var("ERIKA_SR_D3D12_LINALG_CHANNEL_BLOCK") {
        if let Ok(block @ (16 | 32 | 64)) = block.parse::<usize>() {
            g.block = g.block.min(block);
        }
    }
    let direct = match std::env::var("ERIKA_SR_D3D12_LINALG_DIRECT_WEIGHTS").as_deref() {
        Ok("1") => true,
        Ok("0") => false,
        _ => layer.out_channels >= 64,
    };
    // This preview's runtime validator requires Align=128 even for wave
    // descriptor loads. Only use direct loads when every starting address
    // is provably aligned: column starts are 16*REDUCTION half elements,
    // each K step is 64 half elements, and input segment bases are aligned.
    let direct_aligned = if layer.inputs.iter().all(|i| i.kernel == 1) {
        let reduction = layer
            .inputs
            .iter()
            .map(|i| model.tensors[i.tensor])
            .sum::<usize>()
            .div_ceil(16)
            * 16;
        matrix_k(reduction) == 64 || reduction <= 16
    } else {
        let mut base = 0;
        layer.inputs.iter().all(|i| {
            let c = model.tensors[i.tensor];
            let count = (c * i.kernel * i.kernel).div_ceil(16) * 16;
            let aligned = base % 64 == 0 && (matrix_k(c) == 64 || count <= 16);
            base += count;
            aligned
        })
    };
    if streaming && direct && g.col_major && direct_aligned {
        g.direct_weights = true;
        g.stream_weights = false;
        g.shared_weights = false;
    }
    let mut count: usize = layer
        .inputs
        .iter()
        .map(|i| model.tensors[i.tensor] * i.kernel * i.kernel)
        .sum();
    if !layer.inputs.iter().all(|i| i.kernel == 1) {
        count = layer
            .inputs
            .iter()
            .map(|i| (model.tensors[i.tensor] * i.kernel * i.kernel).div_ceil(16) * 16)
            .sum();
    }
    if layer.output == model.head.tensor
        && count.div_ceil(16) * 16 <= 4096
        && std::env::var("ERIKA_SR_D3D12_LINALG_SCALAR_HEAD").as_deref() != Ok("0")
    {
        let mut h = std::env::var("ERIKA_SR_D3D12_LINALG_HEAD_HEIGHT")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|h| matches!(h, 4 | 8 | 16))
            .unwrap_or(16);
        while h >= 2 {
            let bytes: usize = sources(model, layer)
                .iter()
                .map(|&(_, r)| (g.head_width + 2 * r) * (h + 2 * r) * 16)
                .max()
                .unwrap_or(0);
            if bytes <= budget as usize {
                g.waves = 1;
                g.fragments = h;
                g.block = 8;
                g.n = 8;
                g.shared_weights = false;
                g.stream_weights = false;
                g.scalar_head = true;
                g.shared_epilogue = false;
                return g;
            }
            h /= 2;
        }
    }
    if g.n == 8 {
        g.block = 8;
    }
    loop {
        let reduction = if layer.inputs.iter().all(|i| i.kernel == 1) {
            layer
                .inputs
                .iter()
                .map(|i| model.tensors[i.tensor])
                .sum::<usize>()
                .div_ceil(16)
                * 16
        } else {
            layer
                .inputs
                .iter()
                .map(|i| (model.tensors[i.tensor] * i.kernel * i.kernel).div_ceil(16) * 16)
                .sum()
        };
        let raw = if layer.inputs.iter().all(|i| i.kernel == 1) {
            let k = layer
                .inputs
                .iter()
                .map(|i| model.tensors[i.tensor])
                .sum::<usize>()
                .div_ceil(16)
                * 16;
            let pitch = source_pitch(k);
            (16 * pitch - (pitch - k)) * g.height() * 2
        } else {
            sources(model, layer)
                .iter()
                .map(|&(t, r)| {
                    let c = model.tensors[t];
                    let pitch = source_pitch(c);
                    ((16 + 2 * r) * pitch - (pitch - c)) * (g.height() + 2 * r) * 2
                })
                .sum::<usize>()
        } + 64;
        let extra = if g.shared_epilogue && !g.first_im2col {
            let (t, r) = sources(model, layer)
                .into_iter()
                .find(|&(t, _)| model.tensors[t] % 16 == 0)
                .unwrap();
            let c = model.tensors[t];
            let raw =
                ((16 + 2 * r) * source_pitch(c) - (source_pitch(c) - c)) * (g.height() + 2 * r) * 2;
            (g.waves * 16 * g.block * 4).saturating_sub(raw)
        } else {
            0
        };
        let panel = if g.first_im2col {
            (16 * g.height() * source_pitch(reduction) * 2).max(if g.shared_epilogue {
                g.waves * 16 * g.block * 4
            } else {
                0
            })
        } else {
            0
        };
        let raw = raw + extra + panel;
        let cached = if g.shared_weights {
            let available = (budget as usize).saturating_sub(raw) + 16;
            let k = if g.col_major {
                (available / (g.block * 2)).saturating_sub(source_pitch(16) - 16)
            } else {
                available / (source_pitch(g.block) * 2)
            };
            (k / 16 * 16).min(reduction)
        } else {
            0
        };
        let phase = if g.stream_weights {
            let kk = if layer.inputs.iter().all(|i| i.kernel == 1) {
                matrix_k(reduction)
            } else {
                layer
                    .inputs
                    .iter()
                    .map(|i| matrix_k(model.tensors[i.tensor]))
                    .max()
                    .unwrap()
            };
            let (pitch, columns) = if g.col_major {
                (source_pitch(kk), g.block)
            } else {
                (source_pitch(g.block), kk)
            };
            (pitch * columns - (pitch - if g.col_major { kk } else { g.block })) * 2
        } else {
            0
        };
        if raw + phase <= budget as usize && (!g.shared_weights || cached >= reduction) {
            g.cached_k = cached;
            // Partial caches would require descriptor loads at K16/K32
            // offsets that cannot promise the preview's 128-byte alignment.
            if !g.shared_weights && !g.stream_weights && !g.direct_weights {
                g.legacy = true;
            }
            return g;
        }
        if g.fragments > 1 {
            g.fragments /= 2;
        } else if g.waves > 1 {
            g.waves /= 2;
        } else if g.block > 16 {
            g.block /= 2;
        } else if g.shared_weights {
            g.shared_weights = false;
            g.waves = 4;
            g.fragments = if layer.out_channels <= 16 { 4 } else { 2 };
        } else {
            g.legacy = true;
            return g;
        }
    }
}
fn layout(model: &SrModel) -> Result<Layout> {
    layout_for(model, 32768, true)
}
fn layout_for(model: &SrModel, shared_budget: u32, n8: bool) -> Result<Layout> {
    model.validate()?;
    if model.layers.last().map(|l| l.output) != Some(model.head.tensor) {
        return Err(fail("head must be produced by the final layer"));
    }
    if model
        .layers
        .iter()
        .flat_map(|l| &l.inputs)
        .any(|i| i.kernel == 0)
    {
        return Err(fail("zero-sized kernel"));
    }
    let halos: Vec<_> = model.tensor_halos().into_iter().map(|h| h as u32).collect();
    let halo = *halos.iter().max().unwrap_or(&0);
    let width = CORE_W + 2 * halo;
    let height = CORE_H + 2 * halo;
    let geometry: Vec<_> = model
        .layers
        .iter()
        .map(|l| geometry(model, l, shared_budget, n8))
        .collect();
    let pitches: Vec<_> = model
        .tensors
        .iter()
        .map(|&c| (c as u32).div_ceil(2) * 2)
        .collect();
    let mut last_use = vec![0; model.tensors.len()];
    for (i, l) in model.layers.iter().enumerate() {
        for input in &l.inputs {
            last_use[input.tensor] = i;
        }
        if let Some(t) = l.residual {
            last_use[t] = i;
        }
    }
    let mut slot_sizes: Vec<u32> = Vec::new();
    let mut free_after: Vec<usize> = Vec::new();
    let mut tensor_slot = vec![None; model.tensors.len()];
    for (i, l) in model.layers.iter().enumerate() {
        if l.output == model.head.tensor {
            continue;
        }
        let slot = if let Some(s) = free_after.iter().position(|&v| v < i) {
            s
        } else {
            slot_sizes.push(0);
            free_after.push(0);
            slot_sizes.len() - 1
        };
        slot_sizes[slot] = slot_sizes[slot].max(pitches[l.output]);
        free_after[slot] = last_use[l.output].max(i);
        tensor_slot[l.output] = Some(slot);
    }
    let mut slot_channels = 0;
    let slot_offsets: Vec<_> = slot_sizes
        .iter()
        .map(|&c| {
            let p = slot_channels;
            slot_channels += c;
            p
        })
        .collect();
    let offsets = tensor_slot
        .iter()
        .map(|s| s.map_or(0, |s| slot_offsets[s]))
        .collect();
    let bytes = u64::from(width) * u64::from(height) * u64::from(slot_channels.max(1)) * 2;
    if bytes > MAX_SCRATCH {
        return Err(fail("model exceeds 256 MiB tiled scratch limit"));
    }
    Ok(Layout {
        halo,
        offsets,
        halos,
        pitches,
        slot_channels: slot_channels.max(1),
        bytes,
        shared_budget,
        geometry,
    })
}

fn tile_extent(
    layout: &Layout,
    width: u32,
    height: u32,
    budget: u64,
) -> Result<(u32, u32, u32, u32, u64)> {
    let (mut nx, mut ny) = (1, 1);
    loop {
        let cw = width.div_ceil(nx);
        let ch = height.div_ceil(ny);
        let tw = (cw + 2 * layout.halo).min(width);
        let th = (ch + 2 * layout.halo).min(height);
        let bytes = u64::from(tw) * u64::from(th) * u64::from(layout.slot_channels) * 2;
        if bytes <= budget {
            return Ok((cw, ch, tw, th, bytes.max(4)));
        }
        if cw == 1 && ch == 1 {
            return Err(fail("tile halo exceeds scratch budget"));
        }
        if cw >= ch {
            nx += 1;
        } else {
            ny += 1;
        }
    }
}

fn shader_source(
    model: &SrModel,
    layer: &ConvLayer,
    layout: &Layout,
    weights: &mut Vec<f32>,
) -> String {
    use std::fmt::Write;
    let dense = layer.inputs.iter().all(|i| i.kernel == 1);
    let actual: usize = layer
        .inputs
        .iter()
        .map(|i| model.tensors[i.tensor] * i.kernel * i.kernel)
        .sum();
    let reduction: usize = if dense {
        actual.div_ceil(16) * 16
    } else {
        layer
            .inputs
            .iter()
            .map(|i| (model.tensors[i.tensor] * i.kernel * i.kernel).div_ceil(16) * 16)
            .sum()
    };
    let zero = u32::from(model.padding == Padding::Zero);
    let mut source = format!(
        r#"
Texture2D<float> luma : register(t0);
ByteAddressBuffer weights : register(t1);
RWTexture2D<float> outputLuma : register(u0);
RWByteAddressBuffer features : register(u1);
cbuffer Params : register(b0) {{
    uint imageWidth, imageHeight; int originX, originY;
    uint coreX, coreY, coreWidth, coreHeight;
    float offset, scale;
    uint tileWidth, tileHeight;
    uint outputX, outputY, outputWidth, outputHeight;
}};
#define REDUCTION {}
#define OUTPUT_CHANNELS {}
#define HAS_BORDER {}
int2 pixel_position(uint p) {{ return int2(p % outputWidth, p / outputWidth) + int2(outputX, outputY); }}
uint border_variant(int2 p) {{
    uint x = p.x == 0 ? 0 : (p.x == int(imageWidth)-1 ? 2 : 1);
    uint y = p.y == 0 ? 0 : (p.y == int(imageHeight)-1 ? 2 : 1);
    return y*3+x;
}}
float load_tensor(uint tensorOffset, uint channels, uint channel, int2 p, bool input) {{
    {}
    p = clamp(p, int2(0,0), int2(imageWidth,imageHeight)-1);
    if (input) return (luma.Load(int3(p,0))-offset)*scale;
    int2 local = p-int2(originX,originY);
    if (any(local<0) || any(local>=int2(tileWidth,tileHeight))) return 0;
    uint pixel = local.y*tileWidth+local.x;
    return (float)features.Load<float16_t>(2*(tensorOffset*tileWidth*tileHeight+pixel*channels+channel));
}}
float16_t4 load_tensor4(uint tensorOffset,uint channels,uint channel,int2 p) {{
    if({zero} && (any(p<0)||any(p>=int2(imageWidth,imageHeight))))return (float16_t4)0;
    p=clamp(p,int2(0,0),int2(imageWidth,imageHeight)-1);
    int2 local=p-int2(originX,originY);
    if(any(local<0)||any(local>=int2(tileWidth,tileHeight)))return (float16_t4)0;
    uint pixel=local.y*tileWidth+local.x;
    return features.Load<float16_t4>(2*(tensorOffset*tileWidth*tileHeight+pixel*channels+channel));
}}
float fetch_at(int2 p, uint k) {{
"#,
        reduction,
        layer.out_channels,
        u32::from(layer.border.is_some()),
        if model.padding == Padding::Zero {
            "if (any(p<0) || any(p>=int2(imageWidth,imageHeight))) return 0;"
        } else {
            ""
        }
    );
    let mut base = 0;
    for input in &layer.inputs {
        let count = model.tensors[input.tensor] * input.kernel * input.kernel;
        let k = input.kernel;
        // Channel is the inner reduction dimension so neighbouring staging
        // lanes read neighbouring feature values, rather than scattered taps.
        let padded = if dense {
            count
        } else {
            count.div_ceil(16) * 16
        };
        writeln!(source, "if (k < {}) {{ uint q=k-{}; if(q>={count}) return 0; uint ch=q%{}; uint tap=q/{}; float v=load_tensor({}, {}, ch, p+int2(int(tap%{})-{},int(tap/{})-{}), {}); return {}; }}",
            base+padded, base, model.tensors[input.tensor], model.tensors[input.tensor], layout.offsets[input.tensor], layout.pitches[input.tensor], k, k/2, k, k/2, input.tensor==0,
            match input.transform { InputTransform::Identity=>"v", InputTransform::Relu=>"max(v,0.0f)", InputTransform::NegRelu=>"max(-v,0.0f)" }).unwrap();
        base += padded;
    }
    source.push_str("return 0; }\nfloat weight(uint variant, uint channel, uint k) {\n");
    let variants = if layer.border.is_some() { 9 } else { 1 };
    let start = weights.len();
    for variant in 0..variants {
        for (index, input) in layer.inputs.iter().enumerate() {
            let channels = model.tensors[input.tensor];
            let taps = input.kernel * input.kernel;
            // B is KxN row-major. Convert OIHW once on CPU rather than
            // gathering sixteen distant output-channel rows on every wave.
            for tap in 0..taps {
                for c in 0..channels {
                    for channel in 0..layer.out_channels {
                        let values = layer
                            .border
                            .as_ref()
                            .map_or(&input.weights, |v| &v[variant].inputs[index]);
                        weights.push(values[(channel * channels + c) * taps + tap]);
                    }
                }
            }
            if !dense {
                weights.resize(
                    weights.len()
                        + ((channels * taps).div_ceil(16) * 16 - channels * taps)
                            * layer.out_channels,
                    0.0,
                );
            }
        }
        if dense {
            weights.resize(
                weights.len() + (reduction - actual) * layer.out_channels,
                0.0,
            );
        }
    }
    writeln!(
        source,
        "return asfloat(weights.Load(4*({start}+({}*REDUCTION+k)*OUTPUT_CHANNELS+channel))); }}",
        if variants == 9 { "variant" } else { "0" }
    )
    .unwrap();
    let weight_end = weights.len();
    let bias = weights.len();
    for variant in 0..variants {
        weights.extend_from_slice(
            layer
                .border
                .as_ref()
                .map_or(&layer.bias, |v| &v[variant].bias),
        );
    }
    writeln!(source, "float bias(uint variant,uint ch) {{ return asfloat(weights.Load(4*({bias}+{}*OUTPUT_CHANNELS+ch))); }}", if variants==9 {"variant"} else {"0"}).unwrap();
    let activation = match &layer.activation {
        Activation::None => "v".to_string(),
        Activation::Relu => "max(v,0.0f)".to_string(),
        Activation::Silu => "v/(1.0f+exp(-v))".to_string(),
        Activation::Prelu(alpha) => {
            let start = weights.len();
            weights.extend_from_slice(alpha);
            format!("max(v,0.0f)+asfloat(weights.Load(4*({start}+ch)))*min(v,0.0f)")
        }
    };
    writeln!(
        source,
        "float activate(uint ch,float v) {{ return {activation}; }}"
    )
    .unwrap();
    let residual = layer.residual.map_or("0.0f".to_string(), |t| {
        format!(
            "load_tensor({}, {}, ch, p, {})",
            layout.offsets[t],
            layout.pitches[t],
            t == 0
        )
    });
    writeln!(
        source,
        "float residual_at(int2 p,uint ch) {{ return {residual}; }}"
    )
    .unwrap();
    if layer.out_channels % 4 == 0 {
        let variant = if variants == 9 { 4 } else { 0 };
        let residual4 = layer
            .residual
            .map(|t| {
                format!(
                    "v+=(float4)load_tensor4({}, {},ch,p);",
                    layout.offsets[t], layout.pitches[t]
                )
            })
            .unwrap_or_default();
        let activation4 = activation.replace("weights.Load(", "weights.Load4(");
        writeln!(source,"float4 finish4(int2 p,uint ch,float4 v){{v+=asfloat(weights.Load4(4*({bias}+{variant}*OUTPUT_CHANNELS+ch)));{residual4}return {activation4};}}").unwrap();
    }
    source.push_str("float raw_at(int2 p) { return luma.Load(int3(clamp(p,int2(0,0),int2(imageWidth,imageHeight)-1),0)); }\n");
    source.push_str("float bilinear(float2 p) { int2 lo=int2(floor(p)); float2 f=frac(p); return lerp(lerp(raw_at(lo),raw_at(lo+int2(1,0)),f.x),lerp(raw_at(lo+int2(0,1)),raw_at(lo+1),f.x),f.y); }\n");
    source.push_str("void emit_at(int2 p,uint ch,float value) {\n");
    if layer.output == model.head.tensor {
        let (sx, sy) = if model.head.order == SubpixelOrder::RowMajor {
            ("ch%2", "ch/2")
        } else {
            ("ch/2", "ch%2")
        };
        writeln!(source, "if (any(p<int2(coreX,coreY)) || any(p>=int2(coreX+coreWidth,coreY+coreHeight))) return; uint sx={sx}, sy={sy}; value=value/scale+{}; outputLuma[p*2+int2(sx,sy)]={};", if model.head.base==HeadBase::Bilinear {"bilinear(float2(p)-0.25f+0.5f*float2(sx,sy))"} else {"offset"}, if model.head.clamp {"saturate(value)"} else {"value"}).unwrap();
    } else {
        writeln!(
            source,
            "p-=int2(originX,originY); uint location=p.y*tileWidth+p.x; features.Store<float16_t>(2*({}*tileWidth*tileHeight+location*{}+ch),(float16_t)value);",
            layout.offsets[layer.output],layout.pitches[layer.output]
        )
        .unwrap();
    }
    source.push_str("}\n");
    source.push_str("// ERIKA_KERNEL_BEGIN\n");
    let index = model
        .layers
        .iter()
        .position(|l| std::ptr::eq(l, layer))
        .unwrap();
    let g = layout.geometry[index];
    if g.legacy {
        source.push_str("float fetch(uint px,uint k){return fetch_at(pixel_position(px),k);}\nfloat residual(uint px,uint ch){return residual_at(pixel_position(px),ch);}\nvoid emit(uint px,uint ch,float v){emit_at(pixel_position(px),ch,v);}\n");
        source.push_str(include_str!("d3d12_linalg/conv.hlsl"));
    } else if g.scalar_head {
        weights.resize(weights.len().div_ceil(64) * 64, 0.0);
        let offset = weights.len() * 4;
        let main = start + if variants == 9 { 4 * reduction * 4 } else { 0 };
        for i in 0..reduction * 4 {
            weights.push(f16_to_f32(f32_to_f16(weights[main + i])));
        }
        writeln!(source, "#define HEAD_CBV_OFFSET {offset}").unwrap();
        source.push_str(&scalar_head_kernel(model, layer, layout, g));
    } else {
        weights.resize(weights.len().div_ceil(32) * 32, 0.0);
        let matrix_offset = weights.len() * 4;
        let stride = layer.out_channels.div_ceil(g.block) * g.block;
        let main = start
            + if variants == 9 {
                4 * reduction * layer.out_channels
            } else {
                0
            };
        for index in (0..reduction * stride).step_by(2) {
            let at = |index: usize| {
                let (k, ch) = if g.col_major {
                    (index % reduction, index / reduction)
                } else {
                    (index / stride, index % stride)
                };
                if ch < layer.out_channels {
                    weights[main + k * layer.out_channels + ch]
                } else {
                    0.0
                }
            };
            let a = at(index);
            let b = at(index + 1);
            weights.push(f32::from_bits(
                u32::from(f32_to_f16(a)) | (u32::from(f32_to_f16(b)) << 16),
            ));
        }
        writeln!(
            source,
            "#define WEIGHT_BASE {matrix_offset}\n#define WEIGHT_CHANNELS {stride}"
        )
        .unwrap();
        source.push_str(&matrix_kernel(model, layer, layout, g));
    }
    let _ = weight_end;
    source
}

fn scalar_head_kernel(model: &SrModel, layer: &ConvLayer, layout: &Layout, g: Geometry) -> String {
    use std::fmt::Write;
    let mut multiply = String::new();
    let mut epilogue = String::new();
    let src = sources(model, layer);
    let dense = layer.inputs.iter().all(|i| i.kernel == 1);
    let mut bases = Vec::new();
    let mut base = 0;
    for input in &layer.inputs {
        bases.push(base);
        let count = model.tensors[input.tensor] * input.kernel * input.kernel;
        base += if dense {
            count
        } else {
            count.div_ceil(16) * 16
        };
    }
    let mut shared = 0;
    for &(t, r) in &src {
        let c = model.tensors[t];
        let pw = g.width() + 2 * r;
        let area = pw * (g.height() + 2 * r);
        let slices = c.div_ceil(4);
        let phase_limit = std::env::var("ERIKA_SR_D3D12_LINALG_HEAD_QUADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|q| matches!(q, 1 | 2 | 4 | 8 | 16))
            .unwrap_or(2);
        let per_phase = ((layout.shared_budget as usize - 64) / (area * 16)).clamp(1, phase_limit);
        shared = shared.max(area * per_phase.min(slices));
        for first in (0..slices).step_by(per_phase) {
            let count = per_phase.min(slices - first);
            multiply.push_str("GroupMemoryBarrierWithGroupSync();\n");
            let raw = if c % 4 == 0 {
                format!(
                    "(float4)load_tensor4({}, {},q*4,at)",
                    layout.offsets[t], layout.pitches[t]
                )
            } else {
                let v: Vec<_> = (0..4).map(|lane| format!("q*4+{lane}<{c} ? (float)(float16_t)load_tensor({}, {},q*4+{lane},at,{}) : 0.0f", layout.offsets[t],layout.pitches[t],t==0)).collect();
                format!("float4({})", v.join(","))
            };
            writeln!(multiply,"for(uint i=lane;i<{};i+=HEAD_THREADS){{uint q={first}+i%{count};uint p=i/{count};int2 at=begin+int2(int(p%{pw})-{r},int(p/{pw})-{r});headInput[(q-{first})*{area}+p]={raw};}}GroupMemoryBarrierWithGroupSync();",count*area).unwrap();
            for q in first..first + count {
                multiply.push('{');
                // A thread loads each cell once and reuses it for four pixels
                // and all readers of this tensor (including CReLU halves).
                for wy in 0..2 + 2 * r {
                    for wx in 0..2 + 2 * r {
                        writeln!(
                            multiply,
                            "float4 s{wy}_{wx}=headInput[{}+(local.y+{wy})*{pw}+local.x+{wx}];",
                            (q - first) * area
                        )
                        .unwrap();
                    }
                }
                for (index, input) in layer
                    .inputs
                    .iter()
                    .enumerate()
                    .filter(|(_, i)| i.tensor == t)
                {
                    let k = input.kernel;
                    let shift = r - k / 2;
                    for ky in 0..k {
                        for kx in 0..k {
                            multiply.push('{');
                            for p in 0..4 {
                                let raw = format!("s{}_{}", shift + ky + p / 2, shift + kx + p % 2);
                                let value = match input.transform {
                                    InputTransform::Identity => raw,
                                    InputTransform::Relu => format!("max({raw},0.0f)"),
                                    InputTransform::NegRelu => format!("max(-{raw},0.0f)"),
                                };
                                writeln!(multiply, "float4 v{p}={value};").unwrap();
                            }
                            for lane in 0..4 {
                                let ch = q * 4 + lane;
                                if ch >= c {
                                    continue;
                                }
                                let weight = bases[index] + (ky * k + kx) * c + ch;
                                let component = ["x", "y", "z", "w"][lane];
                                for p in 0..4 {
                                    writeln!(
                                        multiply,
                                        "a{p}=mad(HW[{weight}],v{p}.{component},a{p});"
                                    )
                                    .unwrap();
                                }
                            }
                            multiply.push('}');
                        }
                    }
                }
                multiply.push('}');
            }
        }
    }
    for p in 0..4 {
        writeln!(epilogue,"{{int2 p=begin+local+int2({},{});if(p.x<int(outputX+outputWidth)&&p.y<int(outputY+outputHeight)&&(!HAS_BORDER||border_variant(p)==4)){{",p%2,p/2).unwrap();
        writeln!(epilogue, "float4 v=finish4(p,0,a{p});").unwrap();
        for ch in 0..4 {
            writeln!(epilogue, "emit_at(p,{ch},v[{ch}]);").unwrap();
        }
        epilogue.push_str("}}\n");
    }
    let body = include_str!("d3d12_linalg/head.hlsl")
        .replace(
            "// @DECLARATIONS@",
            &format!("groupshared float4 headInput[{shared}];"),
        )
        .replace("// @MULTIPLY@", &multiply)
        .replace("// @EPILOGUE@", &epilogue);
    format!(
        "#define HEAD_ROWS {}\n#define HEAD_COLS {}\n#define HEAD_THREADS {}\n{body}",
        g.height() / 2,
        g.width() / 2,
        g.width() / 2 * g.height() / 2
    )
}

fn matrix_components(
    matrix: &str,
    _expected: usize,
    mut body: impl FnMut(&str) -> String,
) -> String {
    // Respect the driver's opaque per-thread component mapping. Explicitly
    // expanding Set operations caused PSO failures in the preview driver.
    format!(
        "[loop]for(uint element=0;element<{matrix}.Length();++element){{ {} }}\n",
        body("element")
    )
}

fn matrix_kernel(model: &SrModel, layer: &ConvLayer, layout: &Layout, g: Geometry) -> String {
    use std::fmt::Write;
    let mut declarations = String::new();
    let mut stage = String::new();
    let mut accum = String::new();
    let mut mma = String::new();
    let mut epilogue = String::new();
    let src = sources(model, layer);
    let dense = layer.inputs.iter().all(|i| i.kernel == 1);
    let result_tensor = if g.shared_epilogue && !g.first_im2col {
        Some(
            src.iter()
                .find(|&&(t, _)| model.tensors[t] % 16 == 0)
                .unwrap()
                .0,
        )
    } else {
        None
    };
    let result_name = if g.shared_epilogue && g.first_im2col {
        Some("firstInput".to_string())
    } else {
        result_tensor.map(|t| format!("source{t}"))
    };
    let total_reduction = if dense {
        layer
            .inputs
            .iter()
            .map(|i| model.tensors[i.tensor])
            .sum::<usize>()
            .div_ceil(16)
            * 16
    } else {
        layer
            .inputs
            .iter()
            .map(|i| (model.tensors[i.tensor] * i.kernel * i.kernel).div_ceil(16) * 16)
            .sum()
    };
    if g.stream_weights {
        let kk = if dense {
            matrix_k(total_reduction)
        } else {
            layer
                .inputs
                .iter()
                .map(|i| matrix_k(model.tensors[i.tensor]))
                .max()
                .unwrap()
        };
        let (pitch, columns, logical) = if g.col_major {
            (source_pitch(kk), g.block, kk)
        } else {
            (source_pitch(g.block), kk, g.block)
        };
        let size = pitch * columns - (pitch - logical);
        writeln!(declarations, "groupshared float16_t phaseWeights[{size}];").unwrap();
    }
    let stage_phase = |base: usize, kk: usize| -> String {
        let count = kk * g.block;
        let pitch = if g.col_major {
            source_pitch(kk)
        } else {
            source_pitch(g.block)
        };
        let (addr, at) = if g.col_major {
            (
                format!("(channelBase+i/{kk})*REDUCTION+{base}+k+i%{kk}"),
                format!("(i/{kk})*{pitch}+i%{kk}"),
            )
        } else {
            (
                format!("({base}+k+i/CHANNEL_BLOCK)*WEIGHT_CHANNELS+channelBase+i%CHANNEL_BLOCK"),
                format!("(i/CHANNEL_BLOCK)*{pitch}+i%CHANNEL_BLOCK"),
            )
        };
        format!(
            "for(uint i=thread*4;i<{count};i+=GROUP_THREADS*4){{float16_t4 v=weights.Load<float16_t4>(WEIGHT_BASE+2*({addr}));[unroll]for(uint j=0;j<4;++j)phaseWeights[{at}+j]=v[j];}}GroupMemoryBarrierWithGroupSync();\n"
        )
    };
    if g.shared_weights {
        let wpitch = if g.col_major {
            source_pitch(g.cached_k)
        } else {
            source_pitch(g.block)
        };
        let padding = if g.col_major {
            wpitch - g.cached_k
        } else {
            wpitch - g.block
        };
        let size = if g.col_major {
            wpitch * g.block - padding
        } else {
            wpitch * g.cached_k - padding
        };
        writeln!(declarations, "groupshared float16_t cachedWeights[{size}];").unwrap();
        let addr = if g.col_major {
            "((channelBase+i/CACHED_K)*REDUCTION+i%CACHED_K)"
        } else {
            "((i/CHANNEL_BLOCK)*WEIGHT_CHANNELS+channelBase+i%CHANNEL_BLOCK)"
        };
        let logical = g.cached_k * g.block;
        let phys = if g.col_major {
            format!("(i/CACHED_K)*{wpitch}+i%CACHED_K")
        } else {
            format!("(i/CHANNEL_BLOCK)*{wpitch}+i%CHANNEL_BLOCK")
        };
        writeln!(stage,"for(uint i=thread*4;i<{logical};i+=GROUP_THREADS*4){{float16_t4 v=weights.Load<float16_t4>(WEIGHT_BASE+2*{addr});[unroll]for(uint j=0;j<4;++j)cachedWeights[{phys}+j]=v[j];}}").unwrap();
    }
    let load_b = |o: usize, base: usize, kk: usize| -> String {
        let ty = if kk == 16 {
            "MatB".to_string()
        } else {
            format!("MatB{kk}")
        };
        let global = if g.col_major {
            format!(
                "{ty}::Load(weights,WEIGHT_BASE+2*((channelBase+{})*REDUCTION+{base}+k),2*REDUCTION,MatrixLayout::ColMajor,128)",
                o * g.n
            )
        } else {
            format!(
                "{ty}::Load(weights,WEIGHT_BASE+2*(({base}+k)*WEIGHT_CHANNELS+channelBase+{}),2*WEIGHT_CHANNELS,MatrixLayout::RowMajor,128)",
                o * g.n
            )
        };
        if g.stream_weights {
            let pitch = if g.col_major {
                source_pitch(kk)
            } else {
                source_pitch(g.block)
            };
            if g.col_major {
                format!(
                    "{ty} b{o}={ty}::Load(phaseWeights,{}*{pitch},{pitch},MatrixLayout::ColMajor);",
                    o * g.n
                )
            } else {
                format!(
                    "{ty} b{o}={ty}::Load(phaseWeights,{},{pitch},MatrixLayout::RowMajor);",
                    o * g.n
                )
            }
        } else if g.shared_weights {
            let wpitch = if g.col_major {
                source_pitch(g.cached_k)
            } else {
                source_pitch(g.block)
            };
            let local = if g.col_major {
                format!(
                    "{ty}::Load(cachedWeights,{}*{wpitch}+{base}+k,{wpitch},MatrixLayout::ColMajor)",
                    o * g.n
                )
            } else {
                format!(
                    "{ty}::Load(cachedWeights,({base}+k)*{wpitch}+{},{wpitch},MatrixLayout::RowMajor)",
                    o * g.n
                )
            };
            if g.cached_k == total_reduction {
                format!("{ty} b{o}={local};")
            } else {
                format!(
                    "{ty} b{o};if({base}+k+{kk}<=CACHED_K){{b{o}={local};}}else{{b{o}={global};}}"
                )
            }
        } else {
            format!("{ty} b{o}={global};")
        }
    };
    if dense {
        let reduction = layer
            .inputs
            .iter()
            .map(|i| model.tensors[i.tensor])
            .sum::<usize>()
            .div_ceil(16)
            * 16;
        let pitch = source_pitch(reduction);
        let row = 16 * pitch - (pitch - reduction);
        let size = row * g.height();
        writeln!(declarations, "groupshared float16_t denseInput[{size}];").unwrap();
        let logical = 16 * g.height() * reduction;
        writeln!(stage,"for(uint i=thread;i<{logical};i+=GROUP_THREADS){{uint px=i/REDUCTION;denseInput[(px/16)*{row}+(px%16)*{pitch}+i%REDUCTION]=(float16_t)fetch_at(begin+int2(px%16,px/16),i%REDUCTION);}}").unwrap();
    } else {
        for &(t, r) in &src {
            let c = model.tensors[t];
            let pw = 16 + 2 * r;
            let ph = g.height() + 2 * r;
            let pitch = source_pitch(c);
            let row = pw * pitch - (pitch - c);
            let size = row * ph;
            if result_tensor == Some(t) {
                let count = size.div_ceil(2).max(g.waves * 16 * g.block);
                writeln!(declarations, "groupshared uint source{t}[{count}];").unwrap();
            } else {
                writeln!(declarations, "groupshared float16_t source{t}[{size}];").unwrap();
            }
            if c % 4 == 0 {
                let count = pw * ph * c / 4;
                let store = if result_tensor == Some(t) {
                    format!(
                        "uint4 bits=f32tof16((float4)v);uint index=((p/{pw})*{row}+(p%{pw})*{pitch}+ch)/2;source{t}[index]=bits.x|(bits.y<<16);source{t}[index+1]=bits.z|(bits.w<<16);"
                    )
                } else {
                    format!(
                        "[unroll]for(uint j=0;j<4;++j)source{t}[(p/{pw})*{row}+(p%{pw})*{pitch}+ch+j]=v[j];"
                    )
                };
                writeln!(stage,"for(uint i=thread;i<{count};i+=GROUP_THREADS){{uint ch=(i*4)%{c};uint p=i/{};int2 at=begin+int2(int(p%{pw})-{r},int(p/{pw})-{r});float16_t4 v=load_tensor4({}, {},ch,at);{store}}}",c/4,layout.offsets[t],layout.pitches[t]).unwrap();
            } else {
                let count = pw * ph * c;
                writeln!(stage,"for(uint i=thread;i<{count};i+=GROUP_THREADS) {{uint c=i%{c};uint p=i/{c};int2 at=begin+int2(int(p%{pw})-{r},int(p/{pw})-{r});source{t}[(p/{pw})*{row}+(p%{pw})*{pitch}+c]=(float16_t)load_tensor({}, {},c,at,{});}}",layout.offsets[t],layout.pitches[t],t==0).unwrap();
            }
        }
    }
    for f in 0..g.fragments {
        for o in 0..g.block / g.n {
            writeln!(accum, "MatC c{f}_{o}=MatC::Splat(0.0f);").unwrap();
        }
    }
    if g.first_im2col {
        let input = &layer.inputs[0];
        let t = input.tensor;
        let k = input.kernel;
        let r = k / 2;
        let pw = 16 + 2 * r;
        let count = k * k;
        let pitch = source_pitch(total_reduction);
        let row = 16 * pitch;
        writeln!(
            declarations,
            "groupshared uint firstInput[{}];",
            (row * g.height() / 2).max(if g.shared_epilogue {
                g.waves * 16 * g.block
            } else {
                0
            })
        )
        .unwrap();
        let value = match input.transform {
            InputTransform::Identity => "v",
            InputTransform::Relu => "max(v,(float16_t)0)",
            InputTransform::NegRelu => "max(-v,(float16_t)0)",
        };
        writeln!(stage,"GroupMemoryBarrierWithGroupSync();for(uint i=thread;i<{};i+=GROUP_THREADS){{uint px=(i*2)/{total_reduction};uint tap=(i*2)%{total_reduction};uint bits=0;[unroll]for(uint j=0;j<2;++j){{uint d=tap+j;float16_t v=d<{count}?source{t}[(px/16+d/{k})*{pw}+px%16+d%{k}]:(float16_t)0;bits|=f32tof16((float)({value}))<<(j*16);}}firstInput[((px/16)*{row}+(px%16)*{pitch}+tap)/2]=bits;}}",16*g.height()*total_reduction/2).unwrap();
    }
    let mut base = 0;
    if dense {
        let kk = matrix_k(total_reduction);
        let aty = if kk == 16 {
            "MatA".to_string()
        } else {
            format!("MatA{kk}")
        };
        writeln!(mma, "[loop] for(uint k=0;k<REDUCTION;k+={kk}){{").unwrap();
        if g.stream_weights {
            mma.push_str(&stage_phase(0, kk));
        }
        if !g.stream_weights && !g.direct_weights {
            for o in 0..g.block / g.n {
                mma.push_str(&load_b(o, 0, kk));
            }
        }
        for f in 0..g.fragments {
            let pitch = source_pitch(total_reduction);
            let row = 16 * pitch - (pitch - total_reduction);
            writeln!(mma,"{aty} a{f}={aty}::Load(denseInput,(wave*{}+{f})*{row}+k,{pitch},MatrixLayout::RowMajor);",g.fragments).unwrap();
            if !g.stream_weights && !g.direct_weights {
                for o in 0..g.block / g.n {
                    writeln!(mma, "c{f}_{o}.MultiplyAccumulate(a{f},b{o});").unwrap();
                }
            }
        }
        if g.stream_weights || g.direct_weights {
            for o in 0..g.block / g.n {
                mma.push('{');
                mma.push_str(&load_b(o, 0, kk));
                for f in 0..g.fragments {
                    writeln!(mma, "c{f}_{o}.MultiplyAccumulate(a{f},b{o});").unwrap();
                }
                mma.push('}');
            }
            if g.stream_weights {
                mma.push_str("GroupMemoryBarrierWithGroupSync();\n");
            }
        }
        mma.push_str("}\n");
    } else {
        for (input_index, input) in layer.inputs.iter().enumerate() {
            let t = input.tensor;
            let c = model.tensors[t];
            let taps = input.kernel * input.kernel;
            let count = c * taps;
            let padded = count.div_ceil(16) * 16;
            let r = src.iter().find(|s| s.0 == t).unwrap().1;
            let shift = r - input.kernel / 2;
            let pw = 16 + 2 * r;
            let pitch = source_pitch(c);
            let row = pw * pitch - (pitch - c);
            let kernel = input.kernel;
            let kk = matrix_k(c);
            let aty = if kk == 16 {
                "MatA".to_string()
            } else {
                format!("MatA{kk}")
            };
            writeln!(mma, "{{ [loop] for(uint k=0;k<{padded};k+={kk}) {{").unwrap();
            if g.stream_weights {
                mma.push_str(&stage_phase(base, kk));
            }
            if !g.stream_weights && !g.direct_weights {
                for o in 0..g.block / g.n {
                    mma.push_str(&load_b(o, base, kk));
                }
            }
            for f in 0..g.fragments {
                if g.first_im2col {
                    let pitch = source_pitch(total_reduction);
                    let row = 16 * pitch;
                    writeln!(mma,"{aty} a{f}={aty}::Load(firstInput,(wave*{}+{f})*{row}+k,{pitch},MatrixLayout::RowMajor);",g.fragments).unwrap();
                } else if c % 16 == 0 {
                    writeln!(mma,"{aty} a{f}={aty}::Load(source{t},((wave*{}+{f}+{shift}+k/{c}/{kernel})*{row}+({shift}+(k/{c})%{kernel})*{pitch}+k%{c}),{pitch},MatrixLayout::RowMajor);",g.fragments).unwrap();
                    if input.transform != InputTransform::Identity {
                        let transform = if input.transform == InputTransform::Relu {
                            "max(v,(float16_t)0)"
                        } else {
                            "max(-v,(float16_t)0)"
                        };
                        mma.push_str(&matrix_components(&format!("a{f}"), 16 * kk / 32, |e| {
                            format!("float16_t v=a{f}.Get({e});a{f}.Set({e},{transform});")
                        }));
                    }
                } else {
                    let transform = match input.transform {
                        InputTransform::Identity => "v",
                        InputTransform::Relu => "max(v,(float16_t)0)",
                        InputTransform::NegRelu => "max(-v,(float16_t)0)",
                    };
                    writeln!(mma, "MatA a{f}=MatA::Splat(0.0f);").unwrap();
                    mma.push_str(&matrix_components(&format!("a{f}"),8,|e|format!("uint2 q=a{f}.GetCoordinate({e});uint vindex=k+q.y;uint tap=vindex/{c};float16_t v=vindex<{count} ? source{t}[(wave*{}+{f}+{shift}+tap/{kernel})*{row}+(q.x+{shift}+tap%{kernel})*{pitch}+vindex%{c}]:(float16_t)0;a{f}.Set({e},{transform});",g.fragments)));
                }
                if !g.stream_weights && !g.direct_weights {
                    for o in 0..g.block / g.n {
                        writeln!(mma, "c{f}_{o}.MultiplyAccumulate(a{f},b{o});").unwrap();
                    }
                }
            }
            if g.stream_weights || g.direct_weights {
                for o in 0..g.block / g.n {
                    mma.push('{');
                    mma.push_str(&load_b(o, base, kk));
                    for f in 0..g.fragments {
                        writeln!(mma, "c{f}_{o}.MultiplyAccumulate(a{f},b{o});").unwrap();
                    }
                    mma.push('}');
                }
                if g.stream_weights {
                    mma.push_str("GroupMemoryBarrierWithGroupSync();\n");
                }
            }
            mma.push_str("}}\n");
            base += padded;
            let _ = input_index;
        }
    }
    for f in 0..g.fragments {
        if let Some(ref result) = result_name {
            epilogue.push_str("GroupMemoryBarrierWithGroupSync();\n");
            for o in 0..g.block / g.n {
                writeln!(epilogue,"c{f}_{o}.Store({result},wave*16*CHANNEL_BLOCK+{},CHANNEL_BLOCK,MatrixLayout::RowMajor);",o*g.n).unwrap();
            }
            epilogue.push_str("GroupMemoryBarrierWithGroupSync();\n");
            writeln!(epilogue,"for(uint i=thread;i<{};i+=GROUP_THREADS){{uint px=i/(CHANNEL_BLOCK/4);uint ch=channelBase+(i%(CHANNEL_BLOCK/4))*4;int2 p=begin+int2(px%16,(px/16)*{}+{f});if(ch<OUTPUT_CHANNELS&&p.x<int(outputX+outputWidth)&&p.y<int(outputY+outputHeight)&&(!HAS_BORDER||border_variant(p)==4)){{",g.waves*16*g.block/4,g.fragments).unwrap();
            writeln!(epilogue,"#ifdef ACCUMULATE_HALF\nuint lo={result}[i*2],hi={result}[i*2+1];float4 v=f16tof32(uint4(lo&65535,lo>>16,hi&65535,hi>>16));\n#else\nfloat4 v=asfloat(uint4({result}[i*4],{result}[i*4+1],{result}[i*4+2],{result}[i*4+3]));\n#endif\nv=finish4(p,ch,v);").unwrap();
            if layer.output == model.head.tensor {
                epilogue.push_str("[unroll]for(uint j=0;j<4;++j)emit_at(p,ch+j,v[j]);}}\n");
            } else {
                writeln!(epilogue,"p-=int2(originX,originY);features.Store<float16_t4>(2*({}*tileWidth*tileHeight+(p.y*tileWidth+p.x)*{}+ch),(float16_t4)v);}}}}",layout.offsets[layer.output],layout.pitches[layer.output]).unwrap();
            }
            continue;
        }
        for o in 0..g.block / g.n {
            let matrix = format!("c{f}_{o}");
            epilogue.push_str(&matrix_components(&matrix,16*g.n/32,|e|format!("uint2 q={matrix}.GetCoordinate({e});float value=(float){matrix}.Get({e});uint ch=channelBase+{}+q.y;int2 p=begin+int2(q.x,wave*{}+{f});if(ch<OUTPUT_CHANNELS&&p.x<int(outputX+outputWidth)&&p.y<int(outputY+outputHeight)&&(!HAS_BORDER||border_variant(p)==4)){{value=activate(ch,value+bias(4,ch)+residual_at(p,ch));emit_at(p,ch,value);}}",o*g.n,g.fragments)));
        }
    }
    let mut code = format!(
        "\n#define MATRIX_N {}\n#define CHANNEL_BLOCK {}\n#define GROUP_THREADS {}\n#define GROUP_HEIGHT {}\n#define CACHED_K {}\n",
        g.n,
        g.block,
        g.waves * 32,
        g.height(),
        g.cached_k
    );
    let main = include_str!("d3d12_linalg/matrix.hlsl")
        .replace("// @DECLARATIONS@", &declarations)
        .replace("// @STAGE@", &stage)
        .replace("// @ACCUMULATORS@", &accum)
        .replace("// @MULTIPLY@", &mma)
        .replace("// @EPILOGUE@", &epilogue);
    code.push_str(&main);
    code
}

fn compute_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    code: &[u8],
    name: &str,
) -> Result<ID3D12PipelineState> {
    let diagnostics = if std::env::var("ERIKA_SR_D3D12_LINALG_VALIDATION").as_deref() == Ok("1") {
        device.cast::<ID3D12InfoQueue>().ok().map(|q| {
            let count = unsafe { q.GetNumStoredMessages() };
            (q, count)
        })
    } else {
        None
    };
    let mut desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root.clone())),
        CS: D3D12_SHADER_BYTECODE {
            pShaderBytecode: code.as_ptr() as _,
            BytecodeLength: code.len(),
        },
        ..Default::default()
    };
    let result = unsafe { win(device.CreateComputePipelineState(&desc), name) };
    if let Some((q, first)) = diagnostics {
        unsafe {
            for i in first..q.GetNumStoredMessages() {
                let mut size = 0;
                if q.GetMessage(i, None, &mut size).is_ok() {
                    let mut storage = vec![0u64; size.div_ceil(8)];
                    let message = storage.as_mut_ptr() as *mut D3D12_MESSAGE;
                    if q.GetMessage(i, Some(message), &mut size).is_ok()
                        && (*message).Severity.0 <= 1
                    {
                        eprintln!(
                            "D3D12 pipeline diagnostic: {name}, message_id={:?}: {}",
                            (*message).ID,
                            String::from_utf8_lossy(std::slice::from_raw_parts(
                                (*message).pDescription as *const u8,
                                (*message).DescriptionByteLength
                            ))
                        );
                    }
                }
            }
        }
    }
    unsafe {
        ManuallyDrop::drop(&mut desc.pRootSignature);
    }
    result
}
fn create_root(device: &ID3D12Device) -> Result<ID3D12RootSignature> {
    let ranges = [
        D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            ..Default::default()
        },
        D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            ..Default::default()
        },
    ];
    let parameters = [
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: 1,
                    pDescriptorRanges: &ranges[0],
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: 1,
                    pDescriptorRanges: &ranges[1],
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: 1,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_SRV,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: 1,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 16,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
        D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: 1,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        },
    ];
    unsafe {
        let desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: parameters.len() as u32,
            pParameters: parameters.as_ptr(),
            ..Default::default()
        };
        let mut blob = None;
        win(
            D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None),
            "serialize root signature",
        )?;
        let blob = some(blob, "root signature blob")?;
        win(
            device.CreateRootSignature(
                0,
                std::slice::from_raw_parts(
                    blob.GetBufferPointer() as *const u8,
                    blob.GetBufferSize(),
                ),
            ),
            "create root signature",
        )
    }
}
fn buffer(
    device: &ID3D12Device,
    bytes: u64,
    heap: D3D12_HEAP_TYPE,
    flags: D3D12_RESOURCE_FLAGS,
) -> Result<ID3D12Resource> {
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: bytes,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let state = if heap == D3D12_HEAP_TYPE_UPLOAD {
        D3D12_RESOURCE_STATE_GENERIC_READ
    } else if heap == D3D12_HEAP_TYPE_READBACK {
        D3D12_RESOURCE_STATE_COPY_DEST
    } else {
        // Default-heap buffers promote from COMMON on first use and decay
        // after execution. The preview debug layer ignores other initial
        // buffer states and warns about them.
        D3D12_RESOURCE_STATE_COMMON
    };
    let mut result = None;
    unsafe {
        win(
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: heap,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                state,
                None,
                &mut result,
            ),
            "allocate buffer",
        )?;
    }
    some(result, "buffer")
}

struct SharedTexture {
    resource: ID3D12Resource,
    _texture11: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    uav: ID3D11UnorderedAccessView,
}
fn shared_texture(
    d12: &ID3D12Device,
    d11: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
) -> Result<SharedTexture> {
    unsafe {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: width as u64,
            Height: height,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            // D3D11 import of this UAV texture also requires render-target
            // compatibility on the tested NVIDIA driver (otherwise E_INVALIDARG).
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
                | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
                | D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS,
            ..Default::default()
        };
        let mut resource = None;
        win(
            d12.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_SHARED,
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut resource,
            ),
            "allocate shared texture",
        )?;
        let resource = some(resource, "shared texture")?;
        let handle = OwnedHandle(win(
            d12.CreateSharedHandle(&resource, None, GENERIC_ALL_ACCESS, PCWSTR::null()),
            "share texture",
        )?);
        let dev1: ID3D11Device1 = win(d11.cast(), "D3D11Device1")?;
        let texture11: ID3D11Texture2D = win(
            dev1.OpenSharedResource1(handle.0),
            "OpenSharedResource1 texture",
        )?;
        let mut srv = None;
        let mut uav = None;
        win(
            d11.CreateShaderResourceView(&texture11, None, Some(&mut srv)),
            "shared texture SRV",
        )?;
        win(
            d11.CreateUnorderedAccessView(&texture11, None, Some(&mut uav)),
            "shared texture UAV",
        )?;
        Ok(SharedTexture {
            resource,
            _texture11: texture11,
            srv: some(srv, "SRV")?,
            uav: some(uav, "UAV")?,
        })
    }
}
struct Slot {
    width: u32,
    height: u32,
    done: u64,
    prepared: Arc<Prepared>,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    input: SharedTexture,
    output: SharedTexture,
    scratch: ID3D12Resource,
    heap: ID3D12DescriptorHeap,
    queries: ID3D12QueryHeap,
    timestamps: ID3D12Resource,
    copy_shader: ID3D11ComputeShader,
    query_count: u32,
    query_layers: Vec<(usize, bool)>,
    core_width: u32,
    core_height: u32,
    tile_width: u32,
    tile_height: u32,
}
impl Slot {
    fn new(
        owner: &D3d12LinalgUpscaler,
        prepared: Arc<Prepared>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        unsafe {
            let d = &owner.device;
            let (core_width, core_height, tile_width, tile_height, scratch_bytes) =
                tile_extent(&prepared.layout, width, height, owner.tensor_budget)?;
            let allocator = win(
                d.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT),
                "command allocator",
            )?;
            let list: ID3D12GraphicsCommandList = win(
                d.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None),
                "command list",
            )?;
            win(list.Close(), "initial command list close")?;
            let input = shared_texture(
                d,
                &owner.source_device,
                width,
                height,
                DXGI_FORMAT_R32_FLOAT,
            )?;
            let output = shared_texture(
                d,
                &owner.source_device,
                width * 2,
                height * 2,
                DXGI_FORMAT_R16_FLOAT,
            )?;
            let scratch = buffer(
                d,
                scratch_bytes,
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            )?;
            let heap: ID3D12DescriptorHeap = win(
                d.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    ..Default::default()
                }),
                "descriptor heap",
            )?;
            let mut cpu = heap.GetCPUDescriptorHandleForHeapStart();
            let srv_desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_FLOAT,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MipLevels: 1,
                        ..Default::default()
                    },
                },
            };
            d.CreateShaderResourceView(&input.resource, Some(&srv_desc), cpu);
            cpu.ptr +=
                d.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let uav_desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                Format: DXGI_FORMAT_R16_FLOAT,
                ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
                ..Default::default()
            };
            d.CreateUnorderedAccessView(&output.resource, None, Some(&uav_desc), cpu);
            let mut queries = None;
            let dispatches = width.div_ceil(core_width)
                * height.div_ceil(core_height)
                * (prepared.model.layers.len()
                    + prepared.borders.iter().filter(|b| b.is_some()).count())
                    as u32;
            let query_count = if owner.profile { 2 + 2 * dispatches } else { 2 };
            win(
                d.CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: query_count,
                        ..Default::default()
                    },
                    &mut queries,
                ),
                "timestamp query heap",
            )?;
            let timestamps = buffer(
                d,
                query_count as u64 * 8,
                D3D12_HEAP_TYPE_READBACK,
                D3D12_RESOURCE_FLAG_NONE,
            )?;
            let copy_shader = copy_shader(&owner.source_device)?;
            Ok(Self {
                width,
                height,
                done: 0,
                prepared,
                allocator,
                list,
                input,
                output,
                scratch,
                heap,
                queries: some(queries, "query heap")?,
                timestamps,
                copy_shader,
                query_count,
                query_layers: Vec::new(),
                core_width,
                core_height,
                tile_width,
                tile_height,
            })
        }
    }
}
fn copy_shader(device: &ID3D11Device) -> Result<ID3D11ComputeShader> {
    let source = include_bytes!("d3d12_linalg/copy.hlsl");
    unsafe {
        let mut blob = None;
        let mut errors = None;
        win(
            Fxc::D3DCompile(
                source.as_ptr() as _,
                source.len(),
                PCSTR::null(),
                None,
                None,
                windows::core::s!("main"),
                windows::core::s!("cs_5_0"),
                0,
                0,
                &mut blob,
                Some(&mut errors),
            ),
            "compile D3D11 luma copy",
        )?;
        let blob = some(blob, "copy shader")?;
        let mut shader = None;
        win(
            device.CreateComputeShader(
                std::slice::from_raw_parts(
                    blob.GetBufferPointer() as *const u8,
                    blob.GetBufferSize(),
                ),
                None,
                Some(&mut shader),
            ),
            "create D3D11 copy shader",
        )?;
        some(shader, "copy shader")
    }
}
fn transition(
    list: &ID3D12GraphicsCommandList,
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
        ..Default::default()
    };
    unsafe {
        list.ResourceBarrier(std::slice::from_ref(&barrier));
        ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
    }
}
fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER::default()),
        },
        ..Default::default()
    };
    unsafe {
        list.ResourceBarrier(&[barrier]);
    }
}

impl D3d12LinalgUpscaler {
    /// Records GPU-only D3D11 -> D3D12 -> D3D11 synchronization. If all three
    /// command allocators are busy, returns backpressure without blocking CPU.
    pub(crate) fn encode(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        luma: &ID3D11ShaderResourceView,
        width: u32,
        height: u32,
        range: LumaRange,
    ) -> Result<ID3D11ShaderResourceView> {
        if device.as_raw() != self.source_device.as_raw() {
            return Err(fail("D3D11 device changed; recreate backend"));
        }
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return Err(fail("input dimensions must be 1..8192"));
        }
        let prepared = self
            .prepared
            .clone()
            .ok_or_else(|| fail("prepare a model before encode"))?;
        unsafe {
            let c4: ID3D11DeviceContext4 = win(context.cast(), "D3D11DeviceContext4 GPU waits")?;
            if win(context.GetDevice(), "context device")?.as_raw() != device.as_raw()
                || win(luma.GetDevice(), "luma device")?.as_raw() != device.as_raw()
            {
                return Err(fail(
                    "context and luma must belong to the original D3D11 device",
                ));
            }
            if context.GetType() != D3D11_DEVICE_CONTEXT_IMMEDIATE {
                return Err(fail("an immediate D3D11 context is required"));
            }
            let mut srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
            luma.GetDesc(&mut srv_desc);
            if srv_desc.ViewDimension != D3D_SRV_DIMENSION_TEXTURE2DARRAY
                || srv_desc.Anonymous.Texture2DArray.ArraySize == 0
            {
                return Err(fail("luma must be a Texture2DArray SRV"));
            }
            let input_texture: ID3D11Texture2D =
                win(luma.GetResource().and_then(|r| r.cast()), "luma Texture2D")?;
            let mut input_desc = D3D11_TEXTURE2D_DESC::default();
            input_texture.GetDesc(&mut input_desc);
            let mip = srv_desc.Anonymous.Texture2DArray.MostDetailedMip;
            if input_desc.Width.checked_shr(mip).unwrap_or(0) < width
                || input_desc.Height.checked_shr(mip).unwrap_or(0) < height
                || input_desc.SampleDesc.Count != 1
            {
                return Err(fail("luma dimensions or sample count invalid"));
            }
            let completed = self.fence12.GetCompletedValue();
            if completed == u64::MAX {
                return Err(fail("D3D12 device removed"));
            }
            // Read only resolved GPU timestamps, never poll/wait in encode.
            if let Some(s) = self
                .slots
                .iter()
                .filter(|s| s.done > 0 && s.done <= completed)
                .max_by_key(|s| s.done)
            {
                let mut mapped = ptr::null_mut();
                win(
                    s.timestamps.Map(
                        0,
                        Some(&D3D12_RANGE {
                            Begin: 0,
                            End: s.query_count as usize * 8,
                        }),
                        Some(&mut mapped),
                    ),
                    "map timestamps",
                )?;
                let times =
                    std::slice::from_raw_parts(mapped as *const u64, s.query_count as usize);
                let frequency = win(self.queue.GetTimestampFrequency(), "timestamp frequency")?;
                if times[1] > times[0] && frequency != 0 {
                    self.duration = Some(Duration::from_secs_f64(
                        (times[1] - times[0]) as f64 / frequency as f64,
                    ));
                    self.last_layers =
                        vec![(Duration::ZERO, Duration::ZERO, 0); s.prepared.model.layers.len()];
                    self.last_dispatches.clear();
                    for (q, &(layer, border)) in s.query_layers.iter().enumerate() {
                        let d = Duration::from_secs_f64(
                            times[3 + 2 * q].saturating_sub(times[2 + 2 * q]) as f64
                                / frequency as f64,
                        );
                        self.last_layers[layer].0 += d;
                        self.last_layers[layer].1 = self.last_layers[layer].1.max(d);
                        self.last_layers[layer].2 += 1;
                        self.last_dispatches.push((layer, border, d));
                    }
                }
                s.timestamps.Unmap(0, Some(&D3D12_RANGE::default()));
            }
            let index = self.slots.iter().position(|s| s.done <= completed);
            let index = if let Some(i) = index {
                i
            } else {
                if self.slots.len() == MAX_SLOTS {
                    return Err(fail(
                        "three frames in flight; retry on a later renderer tick",
                    ));
                }
                self.slots
                    .push(Slot::new(self, prepared.clone(), width, height)?);
                self.slots.len() - 1
            };
            if self.slots[index].width != width
                || self.slots[index].height != height
                || !Arc::ptr_eq(&self.slots[index].prepared, &prepared)
            {
                self.slots[index] = Slot::new(self, prepared.clone(), width, height)?;
            }
            let slot = &mut self.slots[index];
            win(slot.allocator.Reset(), "reset completed allocator")?;
            win(slot.list.Reset(&slot.allocator, None), "reset command list")?;
            let list = &slot.list;
            let upload_weights = !prepared.uploaded.load(Ordering::Relaxed);
            if upload_weights {
                list.CopyBufferRegion(
                    &prepared.weights,
                    0,
                    &prepared.upload,
                    0,
                    prepared.weights.GetDesc().Width,
                );
                transition(
                    list,
                    &prepared.weights,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE
                        | D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER,
                );
            }
            transition(
                list,
                &slot.input.resource,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            );
            transition(
                list,
                &slot.output.resource,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
            list.EndQuery(&slot.queries, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            list.SetDescriptorHeaps(&[Some(slot.heap.clone())]);
            list.SetComputeRootSignature(&self.root);
            let gpu = slot.heap.GetGPUDescriptorHandleForHeapStart();
            list.SetComputeRootDescriptorTable(0, gpu);
            list.SetComputeRootDescriptorTable(
                1,
                D3D12_GPU_DESCRIPTOR_HANDLE {
                    ptr: gpu.ptr
                        + self.device.GetDescriptorHandleIncrementSize(
                            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                        ) as u64,
                },
            );
            list.SetComputeRootUnorderedAccessView(2, slot.scratch.GetGPUVirtualAddress());
            list.SetComputeRootShaderResourceView(3, prepared.weights.GetGPUVirtualAddress());
            let (offset, scale) = input_normalization(&prepared.model, range);
            slot.query_layers.clear();
            for y in (0..height).step_by(slot.core_height as usize) {
                for x in (0..width).step_by(slot.core_width as usize) {
                    let cw = slot.core_width.min(width - x);
                    let ch = slot.core_height.min(height - y);
                    let mut params = [
                        width,
                        height,
                        x.saturating_sub(prepared.layout.halo),
                        y.saturating_sub(prepared.layout.halo),
                        x,
                        y,
                        cw,
                        ch,
                        offset.to_bits(),
                        scale.to_bits(),
                        slot.tile_width,
                        slot.tile_height,
                        0,
                        0,
                        0,
                        0,
                    ];
                    for (layer_index, (layer, pipeline)) in prepared
                        .model
                        .layers
                        .iter()
                        .zip(&prepared.pipelines)
                        .enumerate()
                    {
                        list.SetPipelineState(pipeline);
                        if let Some(offset) = prepared.head_cbvs[layer_index] {
                            list.SetComputeRootConstantBufferView(
                                5,
                                prepared.weights.GetGPUVirtualAddress() + offset,
                            );
                        }
                        let q = 2 + slot.query_layers.len() as u32 * 2;
                        if self.profile {
                            list.EndQuery(&slot.queries, D3D12_QUERY_TYPE_TIMESTAMP, q);
                        }
                        let halo = prepared.layout.halos[layer.output];
                        let x0 = x.saturating_sub(halo);
                        let y0 = y.saturating_sub(halo);
                        let ow = (x + cw + halo).min(width) - x0;
                        let oh = (y + ch + halo).min(height) - y0;
                        params[12..].copy_from_slice(&[x0, y0, ow, oh]);
                        list.SetComputeRoot32BitConstants(4, 16, params.as_ptr() as _, 0);
                        let g = prepared.layout.geometry[layer_index];
                        if g.legacy {
                            list.Dispatch(
                                (ow * oh).div_ceil(16).min(65535),
                                (layer.out_channels as u32).div_ceil(16),
                                (ow * oh).div_ceil(16).div_ceil(65535),
                            );
                        } else {
                            list.Dispatch(
                                ow.div_ceil(g.width() as u32),
                                oh.div_ceil(g.height() as u32),
                                (layer.out_channels as u32).div_ceil(g.block as u32),
                            );
                        }
                        uav_barrier(list);
                        if self.profile {
                            list.EndQuery(&slot.queries, D3D12_QUERY_TYPE_TIMESTAMP, q + 1);
                            slot.query_layers.push((layer_index, false));
                        }
                        if let Some(border) = &prepared.borders[layer_index] {
                            let q = 2 + slot.query_layers.len() as u32 * 2;
                            if self.profile {
                                list.EndQuery(&slot.queries, D3D12_QUERY_TYPE_TIMESTAMP, q);
                            }
                            list.SetPipelineState(border);
                            let perimeter = width * (1 + u32::from(height > 1))
                                + height.saturating_sub(2) * (1 + u32::from(width > 1));
                            let groups = (perimeter * layer.out_channels as u32).div_ceil(64);
                            list.Dispatch(groups.min(65535), groups.div_ceil(65535), 1);
                            uav_barrier(list);
                            if self.profile {
                                list.EndQuery(&slot.queries, D3D12_QUERY_TYPE_TIMESTAMP, q + 1);
                                slot.query_layers.push((layer_index, true));
                            }
                        }
                    }
                }
            }
            list.EndQuery(&slot.queries, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            list.ResolveQueryData(
                &slot.queries,
                D3D12_QUERY_TYPE_TIMESTAMP,
                0,
                slot.query_count,
                &slot.timestamps,
                0,
            );
            transition(
                list,
                &slot.input.resource,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            );
            transition(
                list,
                &slot.output.resource,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COMMON,
            );
            win(list.Close(), "close inference commands")?;
            let command: ID3D12CommandList = win(list.cast(), "command list interface")?;
            let ready = self
                .timeline
                .checked_add(1)
                .ok_or_else(|| fail("fence timeline overflow"))?;
            let done = ready + 1;
            // CS state is owned by the renderer during this call, like the
            // existing D3D11 upscaler. Unbind all copy resources afterwards.
            context.CSSetShader(&slot.copy_shader, None);
            context.CSSetShaderResources(0, Some(&[Some(luma.clone())]));
            context.CSSetUnorderedAccessViews(
                0,
                1,
                Some([Some(slot.input.uav.clone())].as_ptr()),
                None,
            );
            context.Dispatch(width.div_ceil(8), height.div_ceil(8), 1);
            context.CSSetShaderResources(0, Some(&[None]));
            context.CSSetUnorderedAccessViews(0, 1, Some([None].as_ptr()), None);
            context.CSSetShader(None, None);
            win(c4.Signal(&self.fence11, ready), "D3D11 signal input ready")?;
            context.Flush(); // submits D3D11 commands; does not wait on CPU
            win(self.queue.Wait(&self.fence12, ready), "D3D12 wait input")?;
            self.queue.ExecuteCommandLists(&[Some(command)]);
            if upload_weights {
                prepared.uploaded.store(true, Ordering::Relaxed);
            }
            // Mark in flight before any fallible synchronization operation.
            slot.done = done;
            self.timeline = done;
            win(
                self.queue.Signal(&self.fence12, done),
                "D3D12 signal completion",
            )?;
            win(c4.Wait(&self.fence11, done), "D3D11 wait inference")?;
            self.context = Some(c4);
            Ok(slot.output.srv.clone())
        }
    }
}
impl Drop for D3d12LinalgUpscaler {
    fn drop(&mut self) {
        // Shutdown is allowed to drain. No CPU waits occur in steady state.
        if let Some(context) = &self.context {
            unsafe {
                context.Flush();
            }
        }
        let target = self.slots.iter().map(|s| s.done).max().unwrap_or(0);
        while unsafe { self.fence12.GetCompletedValue() } < target {
            if unsafe { self.device.GetDeviceRemovedReason() }.is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        pipeline::LumaUpscalerMode,
        sr::{catalog::execution_model, reference},
    };
    use super::*;
    use std::time::Instant;

    fn validation_errors(backend: &D3d12LinalgUpscaler) {
        if std::env::var("ERIKA_SR_D3D12_LINALG_VALIDATION").as_deref() != Ok("1") {
            return;
        }
        unsafe {
            let queue: ID3D12InfoQueue = backend.device.cast().unwrap();
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0usize;
                queue.GetMessage(i, None, &mut size).unwrap();
                // u64 backing storage provides sufficient struct alignment.
                let mut storage = vec![0u64; size.div_ceil(8)];
                let message = storage.as_mut_ptr() as *mut D3D12_MESSAGE;
                queue.GetMessage(i, Some(message), &mut size).unwrap();
                let message = &*message;
                let text = std::slice::from_raw_parts(
                    message.pDescription as *const u8,
                    message.DescriptionByteLength,
                );
                eprintln!(
                    "D3D12 validation {:?}: {}",
                    message.Severity,
                    String::from_utf8_lossy(text)
                );
                assert!(
                    message.Severity != D3D12_MESSAGE_SEVERITY_ERROR
                        && message.Severity != D3D12_MESSAGE_SEVERITY_CORRUPTION,
                    "D3D12 validation error"
                );
            }
            queue.ClearStoredMessages();
        }
    }

    fn device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
        unsafe {
            let (mut device, mut context) = (None, None);
            win(
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_FLAG::default(),
                    Some(&[D3D_FEATURE_LEVEL_11_0]),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                ),
                "test hardware D3D11 device",
            )?;
            Ok((some(device, "test device")?, some(context, "test context")?))
        }
    }
    fn backend() -> Option<(ID3D11Device, ID3D11DeviceContext, D3d12LinalgUpscaler)> {
        let (device, context) = match device() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("SKIP linalg: {e}");
                return None;
            }
        };
        match D3d12LinalgUpscaler::new(&device) {
            Ok(backend) => Some((device, context, backend)),
            Err(e) => {
                eprintln!("SKIP preview runtime unavailable: {e}");
                None
            }
        }
    }
    #[test]
    fn preview_capabilities() {
        if let Some((_, _, backend)) = backend() {
            eprintln!("SM 6.10 + native wave32 fp16 16x16x16 matrix multiply available");
            let mut options = [0u32; 5];
            unsafe {
                backend
                    .device
                    .CheckFeatureSupport(D3D12_FEATURE(72), options.as_mut_ptr() as _, 20)
                    .unwrap();
            }
            eprintln!("PREVIEW_OPTIONS {options:?}");
            for n in [4, 8, 16, 32] {
                let mut caps = MatrixCaps {
                    operation: 1,
                    ..Default::default()
                };
                caps.data[..7].copy_from_slice(&[32, 7, 7, 8, 16, 16, n]);
                let result = unsafe {
                    backend.device.CheckFeatureSupport(
                        D3D12_FEATURE(78),
                        &mut caps as *mut _ as _,
                        mem::size_of::<MatrixCaps>() as u32,
                    )
                };
                eprintln!(
                    "MATRIX_SHAPE M=16 K=16 N={n} flags={:#x} result={result:?}",
                    caps.data[7]
                );
            }
        }
    }
    #[test]
    fn preview_shader_compilation() {
        if std::env::var_os("ERIKA_D3D12_DXCOMPILER_PATH").is_none() {
            eprintln!("SKIP shader compile: no configured preview DXC");
            return;
        }
        let dxc = Dxc::new().unwrap();
        for mode in [
            LumaUpscalerMode::ArtCnnC4F16,
            LumaUpscalerMode::ArtCnnC4F32,
            LumaUpscalerMode::ArtCnnR8F64,
            LumaUpscalerMode::Anime4kM,
            LumaUpscalerMode::Anime4kUl,
        ] {
            let model = execution_model(mode).unwrap();
            let layout = layout(&model).unwrap();
            for (i, layer) in model.layers.iter().enumerate() {
                let source = shader_source(&model, layer, &layout, &mut Vec::new());
                for half in [false, true] {
                    dxc.compile(&source, half)
                        .unwrap_or_else(|e| panic!("{} layer {i} half={half}: {e}", model.name));
                }
            }
            eprintln!(
                "compiled {}: {} layers, fp32 and fp16 accumulator, {} MiB scratch",
                model.name,
                model.layers.len(),
                layout.bytes / (1024 * 1024)
            );
        }
    }
    fn input(
        device: &ID3D11Device,
        data: &[f32],
        width: u32,
        height: u32,
    ) -> ID3D11ShaderResourceView {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            };
            let init = D3D11_SUBRESOURCE_DATA {
                pSysMem: data.as_ptr() as _,
                SysMemPitch: width * 4,
                ..Default::default()
            };
            let mut texture = None;
            device
                .CreateTexture2D(&desc, Some(&init), Some(&mut texture))
                .unwrap();
            let desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R32_FLOAT,
                ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                        MipLevels: 1,
                        ArraySize: 1,
                        ..Default::default()
                    },
                },
            };
            let mut srv = None;
            device
                .CreateShaderResourceView(texture.as_ref().unwrap(), Some(&desc), Some(&mut srv))
                .unwrap();
            srv.unwrap()
        }
    }
    fn half(bits: u16) -> f32 {
        let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
        let exponent = (bits >> 10) & 31;
        let mantissa = (bits & 1023) as f32;
        sign * match exponent {
            0 => mantissa * 2.0f32.powi(-24),
            31 => {
                if mantissa == 0.0 {
                    f32::INFINITY
                } else {
                    f32::NAN
                }
            }
            _ => (1.0 + mantissa / 1024.0) * 2.0f32.powi(exponent as i32 - 15),
        }
    }
    fn readback(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        view: &ID3D11ShaderResourceView,
    ) -> Vec<f32> {
        unsafe {
            let resource = view.GetResource().unwrap();
            let texture: ID3D11Texture2D = resource.cast().unwrap();
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            assert_eq!(desc.Format, DXGI_FORMAT_R16_FLOAT);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.MiscFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
            let staging = staging.unwrap();
            context.CopyResource(&staging, &resource);
            let mut map = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))
                .unwrap();
            let mut out = Vec::new();
            for y in 0..desc.Height {
                for x in 0..desc.Width {
                    let address = (map.pData as *const u8).add((y * map.RowPitch + x * 2) as usize);
                    out.push(half(ptr::read_unaligned(address as *const u16)));
                }
            }
            context.Unmap(&staging, 0);
            out
        }
    }
    fn compare(mode: LumaUpscalerMode, large: bool) {
        let Some((device, context, mut backend)) = backend() else {
            return;
        };
        let model = execution_model(mode).unwrap();
        backend.tensor_budget = 64 * 1024;
        assert!(backend.supports(&model));
        backend.prepare(model.clone()).unwrap();
        let sizes = if large {
            vec![(23, 17), (131, 67), (1, 1), (1, 7), (7, 1), (2, 2)]
        } else {
            vec![(13, 9), (1, 1), (2, 2)]
        };
        for (width, height) in sizes {
            let data = reference::test_pattern(width as usize, height as usize, 19);
            let view = input(&device, &data, width, height);
            for range in [LumaRange::Full, LumaRange::Limited] {
                let expected =
                    reference::run(&model, &data, width as usize, height as usize, range);
                let output = backend
                    .encode(&device, &context, &view, width, height, range)
                    .unwrap();
                let actual = readback(&device, &context, &output);
                assert_eq!(actual.len(), expected.len());
                let mut sum = 0.0f64;
                let mut max = 0.0f32;
                for (a, b) in actual.iter().zip(&expected) {
                    assert!(a.is_finite());
                    let e = (a - b).abs();
                    sum += e as f64;
                    max = max.max(e);
                }
                let mae = sum / actual.len() as f64;
                eprintln!(
                    "{} {width}x{height} {range:?}: MAE={mae:.7}, max={max:.7}",
                    model.name
                );
                assert!(mae < 2e-3 && max < 3e-2, "MAE {mae}, max {max}");
            }
        }
        validation_errors(&backend);
    }
    #[test]
    fn oracle_artcnn_c4f16() {
        compare(LumaUpscalerMode::ArtCnnC4F16, true);
    }
    #[test]
    fn oracle_artcnn_c4f32() {
        compare(LumaUpscalerMode::ArtCnnC4F32, true);
    }
    #[test]
    fn oracle_artcnn_r8f64() {
        compare(LumaUpscalerMode::ArtCnnR8F64, false);
    }
    #[test]
    fn oracle_anime4k_m() {
        compare(LumaUpscalerMode::Anime4kM, true);
    }
    #[test]
    fn oracle_anime4k_ul() {
        compare(LumaUpscalerMode::Anime4kUl, true);
    }

    #[test]
    fn custom_graph_semantics() {
        let Some((device, context, mut backend)) = backend() else {
            return;
        };
        for padding in [Padding::Zero, Padding::Clamp] {
            let first = ConvLayer {
                output: 1,
                out_channels: 4,
                inputs: vec![ConvInput {
                    tensor: 0,
                    transform: InputTransform::Identity,
                    kernel: 3,
                    weights: (0..36).map(|i| (i as f32 - 17.0) / 80.0).collect(),
                }],
                bias: vec![-0.2, 0.05, -0.1, 0.15],
                residual: None,
                activation: Activation::Prelu(vec![0.1, 0.2, 0.3, 0.4]),
                border: None,
            };
            let mut variants = Vec::new();
            for class in 0..9 {
                variants.push(LayerWeights {
                    inputs: vec![
                        first.inputs[0]
                            .weights
                            .iter()
                            .map(|v| v * (1.0 + (class as f32 - 4.0) * 0.1))
                            .collect(),
                    ],
                    bias: first
                        .bias
                        .iter()
                        .map(|v| v + (class as f32 - 4.0) * 0.01)
                        .collect(),
                });
            }
            let mut first = first;
            first.border = Some(variants);
            let final_layer = ConvLayer {
                output: 2,
                out_channels: 4,
                inputs: vec![
                    ConvInput {
                        tensor: 1,
                        transform: InputTransform::Relu,
                        kernel: 1,
                        weights: vec![0.05; 16],
                    },
                    ConvInput {
                        tensor: 1,
                        transform: InputTransform::NegRelu,
                        kernel: 3,
                        weights: (0..144).map(|i| (i % 7) as f32 * 0.001).collect(),
                    },
                    ConvInput {
                        tensor: 0,
                        transform: InputTransform::Identity,
                        kernel: 1,
                        weights: vec![0.02; 4],
                    },
                ],
                bias: vec![0.01; 4],
                residual: Some(1),
                activation: Activation::Silu,
                border: None,
            };
            let model = Arc::new(SrModel {
                name: format!("custom_{padding:?}"),
                padding,
                input_range: InputRange::Full,
                tensors: vec![1, 4, 4],
                layers: vec![first, final_layer],
                head: Head {
                    tensor: 2,
                    order: SubpixelOrder::ColumnMajor,
                    base: HeadBase::None,
                    clamp: false,
                },
            });
            backend.prepare(model.clone()).unwrap();
            for (width, height) in [(131, 67), (1, 7), (7, 1), (1, 1)] {
                let data = reference::test_pattern(width as usize, height as usize, 23);
                let view = input(&device, &data, width, height);
                for range in [LumaRange::Full, LumaRange::Limited] {
                    let expected =
                        reference::run(&model, &data, width as usize, height as usize, range);
                    let output = backend
                        .encode(&device, &context, &view, width, height, range)
                        .unwrap();
                    let actual = readback(&device, &context, &output);
                    let max = actual
                        .iter()
                        .zip(expected)
                        .map(|(a, b)| (a - b).abs())
                        .fold(0.0f32, f32::max);
                    assert!(
                        actual.iter().all(|v| v.is_finite()) && max < 2e-3,
                        "custom {padding:?} {width}x{height} {range:?}: {max}"
                    );
                }
            }
        }
        validation_errors(&backend);
    }

    #[test]
    fn unavailable_runtime_is_an_error() {
        // Subprocess environment avoids mutating the parent test process's
        // environment while Rust's test runner may have other tests running.
        let exe = std::env::current_exe().unwrap();
        for case in ["disabled", "missing_sdk", "missing_dll"] {
            let mut command = std::process::Command::new(&exe);
            command.args([
                "--exact",
                "renderer::d3d11_d3d12_linalg::tests::unavailable_runtime_probe",
                "--nocapture",
            ]);
            command.env("ERIKA_LINALG_NEGATIVE_PROBE", case);
            match case {
                "disabled" => {
                    command.env_remove("ERIKA_SR_D3D12_LINALG");
                }
                "missing_sdk" => {
                    command
                        .env("ERIKA_SR_D3D12_LINALG", "1")
                        .env_remove("ERIKA_D3D12_AGILITY_SDK_PATH");
                }
                _ => {
                    command.env("ERIKA_SR_D3D12_LINALG", "1").env(
                        "ERIKA_D3D12_AGILITY_SDK_PATH",
                        std::env::current_dir()
                            .unwrap()
                            .join("_nonexistent_linalg_sdk"),
                    );
                }
            }
            assert!(command.status().unwrap().success(), "negative case {case}");
        }
    }
    #[test]
    fn unavailable_runtime_probe() {
        let Ok(case) = std::env::var("ERIKA_LINALG_NEGATIVE_PROBE") else {
            return;
        };
        let Ok((device, _)) = device() else {
            eprintln!("SKIP negative probe: no D3D11 hardware");
            return;
        };
        let error = match D3d12LinalgUpscaler::new(&device) {
            Ok(_) => panic!("unexpectedly created disabled/unavailable backend"),
            Err(e) => e.to_string(),
        };
        let expected = match case.as_str() {
            "disabled" => "disabled",
            "missing_sdk" => "preview requires",
            _ => "absolute directory containing",
        };
        assert!(error.contains(expected), "{case}: {error}");
        eprintln!("expected graceful failure {case}: {error}");
    }

    // GetData maps S_FALSE to Ok in windows-rs 0.62.2. A zero sentinel is
    // essential; checking only is_ok() would report unfinished timestamps.
    fn query<T: Default>(
        context: &ID3D11DeviceContext,
        query: &ID3D11Query,
        ready: impl Fn(&T) -> bool,
    ) -> T {
        let start = Instant::now();
        loop {
            let mut value = T::default();
            unsafe {
                context
                    .GetData(
                        query,
                        Some(&mut value as *mut _ as _),
                        mem::size_of::<T>() as u32,
                        0,
                    )
                    .unwrap();
            }
            if ready(&value) {
                return value;
            }
            assert!(
                start.elapsed() < Duration::from_secs(120),
                "GPU query timed out"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    #[ignore = "PIX-free per-dispatch GPU profile"]
    fn profile_layers() {
        let Some((device, context, mut backend)) = backend() else {
            return;
        };
        backend.profile = true;
        for mode in benchmark_modes() {
            let model = execution_model(mode).unwrap();
            backend.prepare(model.clone()).unwrap();
            let (width, height) = if mode == LumaUpscalerMode::ArtCnnR8F64 {
                (1280, 720)
            } else {
                (1920, 1080)
            };
            let data = reference::test_pattern(width as usize, height as usize, 1);
            let input = input(&device, &data, width, height);
            let mut samples = Vec::new();
            for iteration in 0..7 {
                let out = backend
                    .encode(&device, &context, &input, width, height, LumaRange::Full)
                    .unwrap();
                let _ = readback(&device, &context, &out);
                if iteration >= 2 {
                    samples.push((
                        backend.duration.unwrap(),
                        backend.last_layers.clone(),
                        backend.last_dispatches.clone(),
                    ));
                }
            }
            samples.sort_by_key(|s| s.0);
            let (duration, layers, dispatches) = samples.swap_remove(samples.len() / 2);
            backend.duration = Some(duration);
            backend.last_layers = layers;
            backend.last_dispatches = dispatches;
            let slot = &backend.slots[0];
            eprintln!(
                "PROFILE {} {width}x{height} network={:?} median_of=5 core={}x{} physical={}x{} scratch_bytes={}",
                model.name,
                backend.duration,
                slot.core_width,
                slot.core_height,
                slot.tile_width,
                slot.tile_height,
                unsafe { slot.scratch.GetDesc().Width }
            );
            for (index, (total, max, count)) in backend.last_layers.iter().enumerate() {
                let layer = &model.layers[index];
                let main: Duration = backend
                    .last_dispatches
                    .iter()
                    .filter(|d| d.0 == index && !d.1)
                    .map(|d| d.2)
                    .sum();
                let border: Duration = backend
                    .last_dispatches
                    .iter()
                    .filter(|d| d.0 == index && d.1)
                    .map(|d| d.2)
                    .sum();
                eprintln!(
                    "LAYER {} {index} out={} channels={} reduction={} total_ms={:.6} max_dispatch_us={:.3} dispatches={count}",
                    model.name,
                    layer.output,
                    layer.out_channels,
                    layer
                        .inputs
                        .iter()
                        .map(|i| model.tensors[i.tensor] * i.kernel * i.kernel)
                        .sum::<usize>(),
                    total.as_secs_f64() * 1000.0,
                    max.as_secs_f64() * 1e6
                );
                eprintln!(
                    "STAGES {} {index} matrix_ms={:.6} border_ms={:.6} geometry={:?}",
                    model.name,
                    main.as_secs_f64() * 1000.0,
                    border.as_secs_f64() * 1000.0,
                    backend.prepared.as_ref().unwrap().layout.geometry[index]
                );
            }
        }
    }
    fn benchmark_modes() -> Vec<LumaUpscalerMode> {
        let filter = std::env::var("ERIKA_SR_D3D12_LINALG_BENCH_MODEL").unwrap_or_default();
        [
            ("c4f16", LumaUpscalerMode::ArtCnnC4F16),
            ("c4f32", LumaUpscalerMode::ArtCnnC4F32),
            ("r8f64", LumaUpscalerMode::ArtCnnR8F64),
            ("m", LumaUpscalerMode::Anime4kM),
            ("ul", LumaUpscalerMode::Anime4kUl),
        ]
        .into_iter()
        .filter(|(name, _)| filter.is_empty() || filter.split(',').any(|f| f == *name))
        .map(|(_, m)| m)
        .collect()
    }
    #[test]
    #[ignore = "GPU benchmark; opt into preview runtime and run with --ignored --nocapture"]
    fn gpu_benchmark() {
        let Some((device, context, mut backend)) = backend() else {
            return;
        };
        for half in [false, true] {
            if let Err(e) = matrix_support(&backend.device, half) {
                eprintln!("SKIP accumulator half={half}: {e}");
                continue;
            }
            backend.half = half;
            for mode in benchmark_modes() {
                let model = execution_model(mode).unwrap();
                backend.prepare(model.clone()).unwrap();
                for (width, height) in [(1280u32, 720u32), (1920, 1080)] {
                    let data = reference::test_pattern(width as usize, height as usize, 1);
                    let view = input(&device, &data, width, height);
                    let mut durations = Vec::new();
                    for iteration in 0..6 {
                        unsafe {
                            let make_query = |kind| {
                                let mut q = None;
                                device
                                    .CreateQuery(
                                        &D3D11_QUERY_DESC {
                                            Query: kind,
                                            MiscFlags: 0,
                                        },
                                        Some(&mut q),
                                    )
                                    .unwrap();
                                q.unwrap()
                            };
                            let disjoint = make_query(D3D11_QUERY_TIMESTAMP_DISJOINT);
                            let begin = make_query(D3D11_QUERY_TIMESTAMP);
                            let end = make_query(D3D11_QUERY_TIMESTAMP);
                            context.Begin(&disjoint);
                            context.End(&begin);
                            let output = backend
                                .encode(&device, &context, &view, width, height, LumaRange::Full)
                                .unwrap();
                            context.End(&end);
                            context.End(&disjoint);
                            context.Flush();
                            let t0 = query::<u64>(&context, &begin, |v| *v != 0);
                            let t1 = query::<u64>(&context, &end, |v| *v != 0);
                            let freq = query::<D3D11_QUERY_DATA_TIMESTAMP_DISJOINT>(
                                &context,
                                &disjoint,
                                |v| v.Frequency != 0,
                            );
                            assert!(!freq.Disjoint.as_bool());
                            // Keep output alive until completion. D3D11 timestamps
                            // include copy and cross-API queue synchronization.
                            drop(output);
                            if iteration > 0 {
                                durations.push((t1 - t0) as f64 / freq.Frequency as f64);
                            }
                        }
                    }
                    durations.sort_by(f64::total_cmp);
                    let seconds = durations[durations.len() / 2];
                    let flops = 2.0 * model.macs_per_pixel() as f64 * width as f64 * height as f64;
                    eprintln!(
                        "BENCH {} {width}x{height} accumulator={} end-to-end median={:.3} ms useful={:.4} TFLOPS network={:?}",
                        model.name,
                        if half { "fp16" } else { "fp32" },
                        seconds * 1000.0,
                        flops / seconds / 1e12,
                        backend.last_gpu_duration()
                    );
                }
            }
        }
    }
}
