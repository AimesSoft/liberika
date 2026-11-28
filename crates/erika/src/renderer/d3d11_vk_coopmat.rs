//! Vulkan KHR cooperative-matrix sidecar for the D3D11 renderer.
//!
//! All interop is on one adapter. D3D copies the view's first array slice to
//! an NT-shared R32F texture, signals a shared fence, then waits on Vulkan's
//! completion value. No CPU fence wait is used by an ordinary encode.

use std::collections::VecDeque;
use std::ffi::{CStr, OsString};
use std::mem;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ash::{Entry, vk};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIDevice, IDXGIResource1,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::core::{Interface, PCSTR, PCWSTR};

use crate::core::{PlayerError, Result};
use crate::renderer::sr::model::{
    Activation, HeadBase, InputTransform, Padding, SrModel, SubpixelOrder, f32_to_f16,
};
use crate::renderer::sr::reference::{LumaRange, input_normalization};

const ARENA_LIMIT: u64 = 240 * 1024 * 1024;
const SUBGROUPS: u32 = 1;
const HEADER: usize = 16;

fn element_epilogues(gpu: &Gpu, model: &SrModel) -> bool {
    gpu.shape[2] == 16
        && model.tensors.iter().all(|&c| c <= 32)
        && std::env::var("ERIKA_VK_ELEMENTS").as_deref() != Ok("0")
        && std::env::var("ERIKA_VK_DIRECT_LOAD").as_deref() != Ok("0")
}

fn fusion_runs(model: &SrModel, shape: [u32; 4]) -> Vec<(usize, usize)> {
    let stages = std::env::var("ERIKA_VK_FUSION")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    if !(1..=3).contains(&stages) || shape != [16, 16, 16, 32] {
        return Vec::new();
    }
    let mut consumers = vec![0; model.tensors.len()];
    for l in &model.layers {
        for i in &l.inputs {
            consumers[i.tensor] += 1;
        }
        if let Some(t) = l.residual {
            consumers[t] += 1;
        }
    }
    let eligible = |i: usize| {
        let l = &model.layers[i];
        [16, 32].contains(&l.out_channels)
            && l.inputs.len() == 1
            && l.inputs[0].kernel == 3
            && l.inputs[0].transform == InputTransform::Identity
            && model.tensors[l.inputs[0].tensor] == l.out_channels
            && l.border.is_none()
            && l.residual.is_none()
            && matches!(l.activation, Activation::None | Activation::Relu)
            && l.output != model.head.tensor
    };
    let mut runs = Vec::new();
    let mut first = 0;
    while first < model.layers.len() {
        if !eligible(first) {
            first += 1;
            continue;
        }
        let mut end = first;
        while end + 1 < model.layers.len()
            && end + 1 - first < stages
            && eligible(end + 1)
            && model.layers[end + 1].inputs[0].tensor == model.layers[end].output
            && model.layers[end + 1].out_channels == model.layers[first].out_channels
            && consumers[model.layers[end].output] == 1
        {
            end += 1;
        }
        if end > first
            || (stages == 1
                && first == 1
                && std::env::var("ERIKA_VK_FUSE_STEM").as_deref() == Ok("1"))
        {
            runs.push((first, end));
        }
        first = end + 1;
    }
    if std::env::var("ERIKA_VK_FUSE_STEM").as_deref() == Ok("1")
        && !runs.is_empty()
        && runs[0].0 == 1
    {
        let s = &model.layers[0];
        if [16, 32].contains(&s.out_channels)
            && s.inputs.len() == 1
            && s.inputs[0].tensor == 0
            && s.inputs[0].kernel == 5
            && s.inputs[0].transform == InputTransform::Identity
            && matches!(s.activation, Activation::Relu)
            && s.residual.is_none()
            && s.out_channels == model.layers[1].out_channels
            && consumers[s.output] == 1
        {
            runs[0].0 = 0;
        }
    }
    runs.retain(|&(first, last)| first < last);
    runs
}

struct FusedPipeline {
    first: usize,
    last: usize,
    pipeline: vk::Pipeline,
    tile: [u32; 2],
}

fn vector_head(gpu: &Gpu, model: &SrModel) -> bool {
    let head_choice = std::env::var("ERIKA_VK_TENSOR_HEAD");
    if head_choice.as_deref() == Ok("1") {
        return false;
    }
    let kernel = model
        .layers
        .last()
        .unwrap()
        .inputs
        .iter()
        .map(|i| i.kernel as u32)
        .max()
        .unwrap();
    // Direct channel-blocked matrix loads outperform the vector head for
    // spatial kernels on the tested adapter; keep vector ALUs for 1x1 heads.
    if head_choice.as_deref() != Ok("0")
        && gpu.shape[2] == 16
        && kernel > 1
        && (kernel > 5
            || model
                .layers
                .last()
                .unwrap()
                .inputs
                .iter()
                .any(|i| i.kernel as u32 != kernel))
    {
        return false;
    }
    let coefficients: usize = model
        .layers
        .last()
        .unwrap()
        .inputs
        .iter()
        .map(|i| model.tensors[i.tensor] * i.kernel * i.kernel)
        .sum();
    coefficients <= 1024
        && gpu.limits.max_uniform_buffer_range >= 16384
        && 16 * (8 + kernel - 1).pow(2) * 4 <= gpu.limits.max_compute_shared_memory_size
}

/// The half-precision spatial head is calibrated against these exact bundled
/// graphs. Comparing the graph, not just its name, also handles custom models
/// that reuse a bundled name without changing their numerical policy.
fn validated_half_head_model(model: &SrModel) -> bool {
    use crate::renderer::pipeline::LumaUpscalerMode;
    use crate::renderer::sr::catalog;

    let mode = match model.name.as_str() {
        "artcnn_c4f16_dn" => LumaUpscalerMode::ArtCnnC4F16Dn,
        "artcnn_c4f32_dn" => LumaUpscalerMode::ArtCnnC4F32Dn,
        _ => return false,
    };
    catalog::execution_model(mode).is_ok_and(|known| model == known.as_ref())
}

fn half_spatial_head(gpu: &Gpu, model: &SrModel) -> bool {
    let choice = std::env::var("ERIKA_VK_FP16_HEAD");
    if choice.as_deref() == Ok("0")
        || (gpu.vendor_id != 0x10de && choice.as_deref() != Ok("1"))
        || !gpu.half_supported
        || !vector_head(gpu, model)
        || std::env::var("ERIKA_VK_SPATIAL_HEAD").as_deref() == Ok("0")
    {
        return false;
    }
    let head = model.layers.last().expect("validated graph has a head");
    let kernel = head.inputs[0].kernel;
    head.inputs.len() <= 16
        && (2..=5).contains(&kernel)
        && head.inputs.iter().all(|input| input.kernel == kernel)
        && validated_half_head_model(model)
}

/// Keep each coefficient quartet in its original 16-byte uniform slot. The
/// shader bitcasts the first two words to four halves; the last two are unused.
fn pack_half_head_weights(weights: &[f32]) -> Vec<[u32; 4]> {
    weights
        .chunks_exact(4)
        .map(|v| {
            [
                u32::from(f32_to_f16(v[0])) | (u32::from(f32_to_f16(v[1])) << 16),
                u32::from(f32_to_f16(v[2])) | (u32::from(f32_to_f16(v[3])) << 16),
                0,
                0,
            ]
        })
        .collect()
}

fn layer_tile(
    gpu: &Gpu,
    model: &SrModel,
    layer: &crate::renderer::sr::model::ConvLayer,
    groups: u32,
    half: bool,
) -> Option<u32> {
    let [m, n, k, _] = gpu.shape;
    let kernel = layer.inputs.iter().map(|i| i.kernel).max()? as u32;
    if layer.output == model.head.tensor && vector_head(gpu, model) {
        return Some(1);
    }
    let align = m.max(k).max(16);
    let rows = (layer.out_channels as u32).div_ceil(align) * align;
    let single = layer.inputs.len() == 1 && layer.inputs[0].tensor == 0;
    {
        for blocks in (1..=4).rev() {
            if rows / m % blocks != 0 {
                continue;
            }
            let shared = k * (n * groups + 8) * 2
                + blocks * m * n * groups * if half { 2 } else { 4 }
                + if single {
                    2
                } else {
                    (k + 8) * kernel * (n * groups + kernel - 1) * 2
                };
            if shared <= gpu.limits.max_compute_shared_memory_size {
                return Some(blocks);
            }
        }
    }
    None
}

fn resident_tile(
    gpu: &Gpu,
    model: &SrModel,
    layer: &crate::renderer::sr::model::ConvLayer,
    groups: u32,
    half: bool,
    requested: u32,
) -> Option<(u32, u32)> {
    if std::env::var("ERIKA_VK_RESIDENT").as_deref() == Ok("0") {
        return None;
    }
    // The 32-channel fp32 case benefits more from the streaming kernel's
    // lower register pressure than from retaining nine weight fragments.
    if layer.out_channels == 32 && !half {
        return None;
    }
    if layer.output == model.head.tensor || (layer.inputs.len() == 1 && layer.inputs[0].tensor == 0)
    {
        return None;
    }
    let kernel = layer.inputs[0].kernel as u32;
    if kernel > 3 || layer.inputs.iter().any(|i| i.kernel as u32 != kernel) {
        return None;
    }
    let [m, n, k, lanes] = gpu.shape;
    let rows = (layer.out_channels as u32).div_ceil(m.max(k).max(16)) * m.max(k).max(16);
    for blocks in [2, 1] {
        if (rows / m) % blocks != 0 {
            continue;
        }
        for strips in [8, 4, 2, 1] {
            if strips > requested {
                continue;
            }
            let registers = blocks * (kernel * kernel * m * k / lanes + strips * m * n / lanes);
            let shared = (k + 8) * kernel * (n * groups * strips + kernel - 1) * 2
                + blocks * m * n * groups * strips * if half { 2 } else { 4 };
            let elements = element_epilogues(gpu, model);
            if registers <= if elements { 256 } else { 192 }
                && (elements || shared <= gpu.limits.max_compute_shared_memory_size)
            {
                return Some((blocks, strips));
            }
        }
    }
    None
}

fn error(message: impl std::fmt::Display) -> PlayerError {
    PlayerError::Renderer(format!("Vulkan cooperative matrix: {message}"))
}

fn bytes<T: Copy>(slice: &[T]) -> &[u8] {
    // All callers use integer/float words or repr(C) structs without padding.
    unsafe { std::slice::from_raw_parts(slice.as_ptr().cast(), mem::size_of_val(slice)) }
}

struct Handle(HANDLE);
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct InstanceOwner(ash::Instance);
impl Drop for InstanceOwner {
    fn drop(&mut self) {
        unsafe {
            self.0.destroy_instance(None);
        }
    }
}

struct Gpu {
    instance: InstanceOwner,
    device: ash::Device,
    physical: vk::PhysicalDevice,
    queue: vk::Queue,
    family: u32,
    memory: vk::PhysicalDeviceMemoryProperties,
    limits: vk::PhysicalDeviceLimits,
    vendor_id: u32,
    timestamp_bits: u32,
    shape: [u32; 4],
    half_supported: bool,
    capture: bool,
    // Fields drop in declaration order: unload after instance destruction.
    _entry: Entry,
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            self.device.destroy_device(None);
        }
    }
}

fn load_vulkan_entry() -> Result<Entry> {
    // App-local loaders can mishandle newer physical-device extension calls,
    // including vkGetPhysicalDeviceCooperativeMatrixPropertiesKHR. Use the
    // system Vulkan runtime; loading errors follow the existing scalar fallback.
    let capacity = unsafe { GetSystemDirectoryW(None) };
    if capacity == 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    let mut directory = vec![0; capacity as usize];
    let length = unsafe { GetSystemDirectoryW(Some(&mut directory)) };
    if length == 0 {
        return Err(error(std::io::Error::last_os_error()));
    }
    directory.truncate(length as usize);
    let path = PathBuf::from(OsString::from_wide(&directory)).join("vulkan-1.dll");
    let entry = unsafe { Entry::load_from(&path) }
        .map_err(|cause| error(format!("cannot load {}: {cause}", path.display())))?;
    crate::trace::diagnostic(
        serde_json::json!({
            "event": "luma_upscaler_vulkan_init",
            "stage": "loader",
            "path": path,
        })
        .to_string(),
    );
    Ok(entry)
}

impl Gpu {
    fn new(d3d: &ID3D11Device) -> Result<Arc<Self>> {
        unsafe {
            let dxgi: IDXGIDevice = d3d.cast().map_err(error)?;
            let adapter = dxgi.GetAdapter().map_err(error)?.GetDesc().map_err(error)?;
            let mut luid = [0u8; 8];
            luid[..4].copy_from_slice(&adapter.AdapterLuid.LowPart.to_le_bytes());
            luid[4..].copy_from_slice(&adapter.AdapterLuid.HighPart.to_le_bytes());
            let entry = load_vulkan_entry()?;
            let app = vk::ApplicationInfo::default()
                .application_name(c"Erika SR")
                .api_version(vk::API_VERSION_1_3);
            let instance = InstanceOwner(
                entry
                    .create_instance(
                        &vk::InstanceCreateInfo::default().application_info(&app),
                        None,
                    )
                    .map_err(error)?,
            );
            let physical = instance
                .0
                .enumerate_physical_devices()
                .map_err(error)?
                .into_iter()
                .find(|&physical| {
                    let mut id = vk::PhysicalDeviceIDProperties::default();
                    let mut properties =
                        vk::PhysicalDeviceProperties2::default().push_next(&mut id);
                    instance
                        .0
                        .get_physical_device_properties2(physical, &mut properties);
                    id.device_luid_valid != 0 && id.device_luid == luid
                })
                .ok_or_else(|| error("no Vulkan device matches the DXGI adapter LUID"))?;
            let mut extensions = vec![
                ash::khr::cooperative_matrix::NAME,
                ash::khr::external_memory_win32::NAME,
                ash::khr::external_semaphore_win32::NAME,
            ];
            let available = instance
                .0
                .enumerate_device_extension_properties(physical)
                .map_err(error)?;
            for &name in &extensions {
                if !available
                    .iter()
                    .any(|e| CStr::from_ptr(e.extension_name.as_ptr()) == name)
                {
                    return Err(error(format!("missing {}", name.to_string_lossy())));
                }
            }
            let capture = cfg!(test)
                && std::env::var_os("ERIKA_VK_STATS").is_some()
                && available.iter().any(|e| {
                    CStr::from_ptr(e.extension_name.as_ptr())
                        == ash::khr::pipeline_executable_properties::NAME
                });
            if capture {
                extensions.push(ash::khr::pipeline_executable_properties::NAME);
            }
            let mut subgroup = vk::PhysicalDeviceSubgroupProperties::default();
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut subgroup);
            instance
                .0
                .get_physical_device_properties2(physical, &mut properties);
            let limits = properties.properties.limits;
            let vendor_id = properties.properties.vendor_id;
            crate::trace::diagnostic(
                serde_json::json!({
                    "event": "luma_upscaler_vulkan_init",
                    "stage": "adapter",
                    "device": CStr::from_ptr(properties.properties.device_name.as_ptr()).to_string_lossy(),
                    "vendor_id": vendor_id,
                    "device_id": properties.properties.device_id,
                    "api_version": properties.properties.api_version,
                    "driver_version": properties.properties.driver_version,
                })
                .to_string(),
            );
            if properties.properties.api_version < vk::API_VERSION_1_3 {
                return Err(error("Vulkan 1.3 is required"));
            }
            let families = instance
                .0
                .get_physical_device_queue_family_properties(physical);
            let family = families
                .iter()
                .position(|p| p.queue_flags.contains(vk::QueueFlags::COMPUTE))
                .ok_or_else(|| error("no compute queue"))? as u32;
            let timestamp_bits = families[family as usize].timestamp_valid_bits;
            let mut coop = vk::PhysicalDeviceCooperativeMatrixFeaturesKHR::default();
            let mut f11 = vk::PhysicalDeviceVulkan11Features::default();
            let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
            let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
            let mut executable =
                vk::PhysicalDevicePipelineExecutablePropertiesFeaturesKHR::default();
            let mut features = vk::PhysicalDeviceFeatures2::default()
                .push_next(&mut coop)
                .push_next(&mut f11)
                .push_next(&mut f12)
                .push_next(&mut f13)
                .push_next(&mut executable);
            instance
                .0
                .get_physical_device_features2(physical, &mut features);
            let base_features = features.features;
            let capture = capture && executable.pipeline_executable_info != 0;
            if base_features.shader_storage_image_extended_formats == 0
                || coop.cooperative_matrix == 0
                || f11.storage_buffer16_bit_access == 0
                || f12.shader_float16 == 0
                || f12.timeline_semaphore == 0
                || f13.maintenance4 == 0
            {
                return Err(error(
                    "requires cooperativeMatrix, shaderFloat16, storageBuffer16BitAccess, timelineSemaphore, maintenance4 and extended storage image formats",
                ));
            }
            let coop_api = ash::khr::cooperative_matrix::Instance::new(&entry, &instance.0);
            let matrices = coop_api
                .get_physical_device_cooperative_matrix_properties(physical)
                .map_err(error)?;
            let suitable = |p: &&vk::CooperativeMatrixPropertiesKHR<'_>| {
                p.scope == vk::ScopeKHR::SUBGROUP
                    && p.a_type == vk::ComponentTypeKHR::FLOAT16
                    && p.b_type == vk::ComponentTypeKHR::FLOAT16
                    && p.saturating_accumulation == 0
                    && p.m_size <= 32
                    && p.n_size <= 32
                    && p.k_size <= 32
            };
            let shape = matrices
                .iter()
                .filter(suitable)
                .filter(|p| {
                    p.c_type == vk::ComponentTypeKHR::FLOAT32
                        && p.result_type == vk::ComponentTypeKHR::FLOAT32
                })
                .min_by_key(|p| {
                    p.m_size.abs_diff(16) + p.n_size.abs_diff(16) + p.k_size.abs_diff(16)
                })
                .ok_or_else(|| {
                    error("no subgroup fp16 A/B, fp32 accumulator matrix configuration")
                })?;
            let shape = [
                shape.m_size,
                shape.n_size,
                shape.k_size,
                subgroup.subgroup_size,
            ];
            if shape[3] == 0 || shape[3] * SUBGROUPS > limits.max_compute_work_group_invocations {
                return Err(error("unsupported compute subgroup width"));
            }
            let half_supported = matrices.iter().filter(suitable).any(|p| {
                [p.m_size, p.n_size, p.k_size] == shape[..3]
                    && p.c_type == vk::ComponentTypeKHR::FLOAT16
                    && p.result_type == vk::ComponentTypeKHR::FLOAT16
            });
            let mut sem_type =
                vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
            let sem_info = vk::PhysicalDeviceExternalSemaphoreInfo::default()
                .handle_type(vk::ExternalSemaphoreHandleTypeFlags::D3D12_FENCE)
                .push_next(&mut sem_type);
            let mut sem_props = vk::ExternalSemaphoreProperties::default();
            instance
                .0
                .get_physical_device_external_semaphore_properties(
                    physical,
                    &sem_info,
                    &mut sem_props,
                );
            if !sem_props
                .external_semaphore_features
                .contains(vk::ExternalSemaphoreFeatureFlags::IMPORTABLE)
            {
                return Err(error(
                    "cannot import a D3D11 shared fence as a D3D12_FENCE timeline",
                ));
            }
            let priorities = [1.0];
            let queues = [vk::DeviceQueueCreateInfo::default()
                .queue_family_index(family)
                .queue_priorities(&priorities)];
            let extension_names: Vec<_> = extensions.iter().map(|n| n.as_ptr()).collect();
            let mut coop =
                vk::PhysicalDeviceCooperativeMatrixFeaturesKHR::default().cooperative_matrix(true);
            let mut f11 =
                vk::PhysicalDeviceVulkan11Features::default().storage_buffer16_bit_access(true);
            let mut f12 = vk::PhysicalDeviceVulkan12Features::default()
                .shader_float16(true)
                .timeline_semaphore(true);
            let mut f13 = vk::PhysicalDeviceVulkan13Features::default().maintenance4(true);
            let base =
                vk::PhysicalDeviceFeatures::default().shader_storage_image_extended_formats(true);
            let mut device_info = vk::DeviceCreateInfo::default()
                .queue_create_infos(&queues)
                .enabled_extension_names(&extension_names)
                .enabled_features(&base)
                .push_next(&mut coop)
                .push_next(&mut f11)
                .push_next(&mut f12)
                .push_next(&mut f13);
            let mut executable =
                vk::PhysicalDevicePipelineExecutablePropertiesFeaturesKHR::default()
                    .pipeline_executable_info(true);
            if capture {
                device_info = device_info.push_next(&mut executable);
            }
            let device = instance
                .0
                .create_device(physical, &device_info, None)
                .map_err(error)?;
            let queue = device.get_device_queue(family, 0);
            let memory = instance.0.get_physical_device_memory_properties(physical);
            Ok(Arc::new(Self {
                _entry: entry,
                instance,
                device,
                physical,
                queue,
                family,
                memory,
                limits,
                vendor_id,
                timestamp_bits,
                shape,
                half_supported,
                capture,
            }))
        }
    }

    fn memory_type(&self, bits: u32, flags: vk::MemoryPropertyFlags) -> Result<u32> {
        (0..self.memory.memory_type_count)
            .find(|&i| {
                bits & (1 << i) != 0
                    && self.memory.memory_types[i as usize]
                        .property_flags
                        .contains(flags)
            })
            .ok_or_else(|| error(format!("no memory type for {flags:?}")))
    }
}

struct Buffer {
    gpu: Arc<Gpu>,
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    size: u64,
}
impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe {
            self.gpu.device.destroy_buffer(self.buffer, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}
impl Buffer {
    fn new(gpu: &Arc<Gpu>, size: u64, upload: Option<&[u8]>) -> Result<Self> {
        let mut out = Self {
            gpu: gpu.clone(),
            buffer: vk::Buffer::null(),
            memory: vk::DeviceMemory::null(),
            size,
        };
        unsafe {
            out.buffer = gpu
                .device
                .create_buffer(
                    &vk::BufferCreateInfo::default().size(size.max(4)).usage(
                        vk::BufferUsageFlags::STORAGE_BUFFER | vk::BufferUsageFlags::UNIFORM_BUFFER,
                    ),
                    None,
                )
                .map_err(error)?;
            let req = gpu.device.get_buffer_memory_requirements(out.buffer);
            let flags = if upload.is_some() {
                vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_COHERENT
            } else {
                vk::MemoryPropertyFlags::DEVICE_LOCAL
            };
            let ty = gpu
                .memory_type(
                    req.memory_type_bits,
                    flags | vk::MemoryPropertyFlags::DEVICE_LOCAL,
                )
                .or_else(|_| gpu.memory_type(req.memory_type_bits, flags))?;
            out.memory = gpu
                .device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(ty),
                    None,
                )
                .map_err(error)?;
            gpu.device
                .bind_buffer_memory(out.buffer, out.memory, 0)
                .map_err(error)?;
            if let Some(data) = upload {
                let ptr = gpu
                    .device
                    .map_memory(out.memory, 0, req.size, vk::MemoryMapFlags::empty())
                    .map_err(error)?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr.cast(), data.len());
                gpu.device.unmap_memory(out.memory);
            }
        }
        Ok(out)
    }
}

struct SharedImage {
    gpu: Arc<Gpu>,
    image: vk::Image,
    memory: vk::DeviceMemory,
    view: vk::ImageView,
    _texture: ID3D11Texture2D,
    srv: ID3D11ShaderResourceView,
    uav: ID3D11UnorderedAccessView,
}
impl Drop for SharedImage {
    fn drop(&mut self) {
        unsafe {
            self.gpu.device.destroy_image_view(self.view, None);
            self.gpu.device.destroy_image(self.image, None);
            self.gpu.device.free_memory(self.memory, None);
        }
    }
}
impl SharedImage {
    fn new(
        gpu: &Arc<Gpu>,
        d3d: &ID3D11Device,
        width: u32,
        height: u32,
        output: bool,
    ) -> Result<Self> {
        unsafe {
            let format = if output {
                vk::Format::R16_SFLOAT
            } else {
                vk::Format::R32_SFLOAT
            };
            let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE);
            let info = vk::PhysicalDeviceImageFormatInfo2::default()
                .format(format)
                .ty(vk::ImageType::TYPE_2D)
                .tiling(vk::ImageTiling::OPTIMAL)
                .usage(vk::ImageUsageFlags::STORAGE)
                .push_next(&mut external);
            let mut external_props = vk::ExternalImageFormatProperties::default();
            let mut props = vk::ImageFormatProperties2::default().push_next(&mut external_props);
            gpu.instance
                .0
                .get_physical_device_image_format_properties2(gpu.physical, &info, &mut props)
                .map_err(error)?;
            if !external_props
                .external_memory_properties
                .external_memory_features
                .contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE)
            {
                return Err(error("D3D11_TEXTURE storage-image import unsupported"));
            }
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: if output {
                    DXGI_FORMAT_R16_FLOAT
                } else {
                    DXGI_FORMAT_R32_FLOAT
                },
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_SHADER_RESOURCE | D3D11_BIND_UNORDERED_ACCESS).0 as u32,
                CPUAccessFlags: 0,
                MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE | D3D11_RESOURCE_MISC_SHARED).0
                    as u32,
            };
            let mut texture = None;
            d3d.CreateTexture2D(&desc, None, Some(&mut texture))
                .map_err(|e| {
                    error(format!(
                        "CreateTexture2D({width}x{height}, {format:?}): {e}"
                    ))
                })?;
            let texture = texture.ok_or_else(|| error("CreateTexture2D returned null"))?;
            let dxgi: IDXGIResource1 = texture.cast().map_err(error)?;
            let handle = Handle(
                dxgi.CreateSharedHandle(
                    None,
                    DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                    PCWSTR::null(),
                )
                .map_err(|e| error(format!("IDXGIResource1::CreateSharedHandle: {e}")))?,
            );
            let mut srv = None;
            let mut uav = None;
            d3d.CreateShaderResourceView(&texture, None, Some(&mut srv))
                .map_err(error)?;
            d3d.CreateUnorderedAccessView(&texture, None, Some(&mut uav))
                .map_err(error)?;
            let mut out = Self {
                gpu: gpu.clone(),
                image: vk::Image::null(),
                memory: vk::DeviceMemory::null(),
                view: vk::ImageView::null(),
                _texture: texture,
                srv: srv.unwrap(),
                uav: uav.unwrap(),
            };
            let mut ext = vk::ExternalMemoryImageCreateInfo::default()
                .handle_types(vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE);
            out.image = gpu
                .device
                .create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(format)
                        .extent(vk::Extent3D {
                            width,
                            height,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(vk::ImageTiling::OPTIMAL)
                        .usage(vk::ImageUsageFlags::STORAGE)
                        .initial_layout(vk::ImageLayout::UNDEFINED)
                        .push_next(&mut ext),
                    None,
                )
                .map_err(error)?;
            let req = gpu.device.get_image_memory_requirements(out.image);
            let mut handle_props = vk::MemoryWin32HandlePropertiesKHR::default();
            ash::khr::external_memory_win32::Device::new(&gpu.instance.0, &gpu.device)
                .get_memory_win32_handle_properties(
                    vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE,
                    handle.0.0 as isize,
                    &mut handle_props,
                )
                .map_err(error)?;
            let ty = gpu.memory_type(
                req.memory_type_bits & handle_props.memory_type_bits,
                vk::MemoryPropertyFlags::DEVICE_LOCAL,
            )?;
            let mut import = vk::ImportMemoryWin32HandleInfoKHR::default()
                .handle_type(vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE)
                .handle(handle.0.0 as isize);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(out.image);
            out.memory = gpu
                .device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(ty)
                        .push_next(&mut import)
                        .push_next(&mut dedicated),
                    None,
                )
                .map_err(error)?;
            gpu.device
                .bind_image_memory(out.image, out.memory, 0)
                .map_err(error)?;
            out.view = gpu
                .device
                .create_image_view(
                    &vk::ImageViewCreateInfo::default()
                        .image(out.image)
                        .format(format)
                        .view_type(vk::ImageViewType::TYPE_2D)
                        .subresource_range(image_range()),
                    None,
                )
                .map_err(error)?;
            Ok(out)
        }
    }
}

fn image_range() -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange::default()
        .aspect_mask(vk::ImageAspectFlags::COLOR)
        .level_count(1)
        .layer_count(1)
}

struct Pipelines {
    gpu: Arc<Gpu>,
    set_layout: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    conv: Vec<vk::Pipeline>,
    border: Vec<vk::Pipeline>,
    input: vk::Pipeline,
    pad: vk::Pipeline,
    layer_groups: Vec<u32>,
    blocks: Vec<u32>,
    vector_head: bool,
    strips: Vec<u32>,
    fused: Vec<FusedPipeline>,
}
impl Drop for Pipelines {
    fn drop(&mut self) {
        unsafe {
            for pipeline in self
                .conv
                .iter()
                .copied()
                .chain(self.border.iter().copied())
                .chain([self.input, self.pad])
                .chain(self.fused.iter().map(|f| f.pipeline))
            {
                self.gpu.device.destroy_pipeline(pipeline, None);
            }
            self.gpu.device.destroy_pipeline_layout(self.layout, None);
            self.gpu
                .device
                .destroy_descriptor_set_layout(self.set_layout, None);
        }
    }
}
impl Pipelines {
    fn new(
        gpu: &Arc<Gpu>,
        half: bool,
        half_head: bool,
        model: &SrModel,
        packed: &PackedModel,
        groups: u32,
        requested_strips: u32,
    ) -> Result<Self> {
        let mut out = Self {
            gpu: gpu.clone(),
            set_layout: vk::DescriptorSetLayout::null(),
            layout: vk::PipelineLayout::null(),
            conv: Vec::new(),
            border: Vec::new(),
            input: vk::Pipeline::null(),
            pad: vk::Pipeline::null(),
            layer_groups: Vec::new(),
            blocks: Vec::new(),
            vector_head: vector_head(gpu, model),
            strips: Vec::new(),
            fused: Vec::new(),
        };
        unsafe {
            let bindings: Vec<_> = (0..6)
                .map(|i| {
                    vk::DescriptorSetLayoutBinding::default()
                        .binding(i)
                        .descriptor_type(if i < 3 {
                            vk::DescriptorType::STORAGE_BUFFER
                        } else if i < 5 {
                            vk::DescriptorType::STORAGE_IMAGE
                        } else {
                            vk::DescriptorType::UNIFORM_BUFFER
                        })
                        .descriptor_count(1)
                        .stage_flags(vk::ShaderStageFlags::COMPUTE)
                })
                .collect();
            out.set_layout = gpu
                .device
                .create_descriptor_set_layout(
                    &vk::DescriptorSetLayoutCreateInfo::default().bindings(&bindings),
                    None,
                )
                .map_err(error)?;
            let layouts = [out.set_layout];
            let ranges = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(mem::size_of::<Params>() as u32)];
            out.layout = gpu
                .device
                .create_pipeline_layout(
                    &vk::PipelineLayoutCreateInfo::default()
                        .set_layouts(&layouts)
                        .push_constant_ranges(&ranges),
                    None,
                )
                .map_err(error)?;
            let mut specializations = Vec::new();
            for (layer, &header) in model.layers.iter().zip(&packed.headers) {
                let groups = if model.tensors.iter().all(|&c| c <= 32)
                    && ((layer.inputs.len() == 1 && layer.inputs[0].tensor == 0)
                        || (layer.output == model.head.tensor && !out.vector_head))
                {
                    std::env::var("ERIKA_VK_EDGE_GROUPS")
                        .ok()
                        .and_then(|v| v.parse::<u32>().ok())
                        .filter(|v| [1, 2, 4, 8].contains(v))
                        .unwrap_or(2)
                } else {
                    groups
                };
                out.layer_groups.push(groups);
                let mut values = gpu.shape.to_vec();
                values.push(layer.inputs.iter().map(|i| i.kernel).max().unwrap() as u32);
                values
                    .extend_from_slice(&packed.metadata[header as usize..header as usize + HEADER]);
                let first = &layer.inputs[0];
                let same_kernel = layer.inputs.iter().all(|i| i.kernel == first.kernel);
                let same_channels = layer
                    .inputs
                    .iter()
                    .all(|i| model.tensors[i.tensor] == model.tensors[first.tensor]);
                let same_transform = layer.inputs.iter().all(|i| i.transform == first.transform);
                let tap = packed.metadata[header as usize + 13] as usize;
                values.extend_from_slice(&[
                    if same_kernel { first.kernel as u32 } else { 0 },
                    if same_channels {
                        packed.metadata[tap + 4]
                    } else {
                        0
                    },
                    if same_transform {
                        packed.metadata[tap + 3]
                    } else {
                        u32::MAX
                    },
                    packed.metadata[tap + 5],
                    packed.metadata[tap],
                ]);
                values.push(groups);
                let resident = resident_tile(gpu, model, layer, groups, half, requested_strips);
                let strips = resident.map_or(1, |(_, s)| s);
                let blocks = if let Some((blocks, _)) = resident {
                    blocks
                } else {
                    layer_tile(gpu, model, layer, groups, half)
                        .ok_or_else(|| error("matrix tile exceeds shared memory limit"))?
                };
                values.push(blocks);
                values.push(u32::from(
                    layer.inputs.len() == 1 && layer.inputs[0].tensor == 0,
                ));
                values.push(if same_channels {
                    model.tensors[first.tensor] as u32
                } else {
                    0
                });
                values.push(strips);
                values.push(u32::from(layer.inputs.len() == 1));
                values.push(packed.coordinates[usize::from(half)]);
                out.blocks.push(blocks);
                out.strips.push(strips);
                specializations.push(values.clone());
                out.border.push(if layer.border.is_some() {
                    out.compile(
                        if std::env::var("ERIKA_VK_BORDER").as_deref() == Ok("1")
                            || (std::env::var("ERIKA_VK_BORDER").as_deref() != Ok("0")
                                && model.tensors.iter().all(|&c| c <= 32))
                        {
                            include_bytes!("vk_coopmat/border_specialized.spv")
                        } else {
                            include_bytes!("vk_coopmat/border.spv")
                        },
                        &values,
                    )?
                } else {
                    vk::Pipeline::null()
                });
                let point_head = layer.output == model.head.tensor
                    && out.vector_head
                    && layer.inputs.len() <= 16
                    && layer.inputs.iter().all(|i| i.kernel == 1)
                    && std::env::var("ERIKA_VK_POINT_HEAD").as_deref() != Ok("0");
                let spatial_head = layer.output == model.head.tensor
                    && out.vector_head
                    && layer.inputs.len() <= 16
                    && first.kernel > 1
                    && first.kernel <= 5
                    && layer.inputs.iter().all(|i| i.kernel == first.kernel)
                    && std::env::var("ERIKA_VK_SPATIAL_HEAD").as_deref() != Ok("0");
                if point_head || spatial_head {
                    values.resize(96, 0);
                    for (i, _) in layer.inputs.iter().enumerate() {
                        let d = tap + i * first.kernel * first.kernel * 8;
                        values[32 + i * 4..36 + i * 4].copy_from_slice(&[
                            packed.metadata[d],
                            packed.metadata[d + 3],
                            packed.metadata[d + 6],
                            packed.metadata[d + 7] & 0x7fffffff,
                        ]);
                    }
                }
                out.conv.push(out.compile(
                    if point_head {
                        include_bytes!("vk_coopmat/head_point.spv")
                    } else if spatial_head && half_head {
                        include_bytes!("vk_coopmat/head_spatial_half.spv")
                    } else if spatial_head {
                        include_bytes!("vk_coopmat/head_spatial.spv")
                    } else if layer.output == model.head.tensor && out.vector_head {
                        include_bytes!("vk_coopmat/head.spv")
                    } else if resident.is_some() {
                        if gpu.shape[2] == 16
                            && std::env::var("ERIKA_VK_DIRECT_LOAD").as_deref() != Ok("0")
                        {
                            if element_epilogues(gpu, model) {
                                if half {
                                    include_bytes!("vk_coopmat/resident_elements_half.spv")
                                } else {
                                    include_bytes!("vk_coopmat/resident_elements.spv")
                                }
                            } else if half {
                                include_bytes!("vk_coopmat/resident_direct_half.spv")
                            } else {
                                include_bytes!("vk_coopmat/resident_direct.spv")
                            }
                        } else if half {
                            include_bytes!("vk_coopmat/resident_half.spv")
                        } else {
                            include_bytes!("vk_coopmat/resident.spv")
                        }
                    } else if gpu.shape[2] == 16
                        && std::env::var("ERIKA_VK_DIRECT_LOAD").as_deref() != Ok("0")
                    {
                        if element_epilogues(gpu, model) {
                            if half {
                                include_bytes!("vk_coopmat/conv_elements_half.spv")
                            } else {
                                include_bytes!("vk_coopmat/conv_elements.spv")
                            }
                        } else if half {
                            include_bytes!("vk_coopmat/conv_direct_half.spv")
                        } else {
                            include_bytes!("vk_coopmat/conv_direct.spv")
                        }
                    } else if half {
                        include_bytes!("vk_coopmat/conv_half.spv")
                    } else {
                        include_bytes!("vk_coopmat/conv.spv")
                    },
                    &values,
                )?);
            }
            out.input = out.compile(include_bytes!("vk_coopmat/input.spv"), &[])?;
            out.pad = out.compile(include_bytes!("vk_coopmat/pad.spv"), &[])?;
            for &(first, last) in &packed.fusions {
                let channels = model.layers[first].out_channels as u32;
                let stages = (last - first + 1) as u32;
                let mut tile = if channels == 16 { [32, 8] } else { [32, 4] };
                if let Ok(v) = std::env::var("ERIKA_VK_FUSION_TILE") {
                    if let Some((w, h)) = v.split_once('x') {
                        if let (Ok(w), Ok(h)) = (w.parse::<u32>(), h.parse::<u32>()) {
                            if w >= 16 && w <= 128 && w % 16 == 0 && h >= 1 && h <= 32 {
                                tile = [w, h];
                            }
                        }
                    }
                }
                let groups = std::env::var("ERIKA_VK_FUSION_GROUPS")
                    .ok()
                    .and_then(|v| v.parse::<u32>().ok())
                    .filter(|v| [1, 2, 4, 8].contains(v))
                    .unwrap_or(4);
                let stem = first == 0 && model.layers[first].inputs[0].tensor == 0;
                let sw = tile[0] + 2 * (stages - 1);
                let sh = tile[1] + 2 * (stages - 1);
                let shared = (sw * sh + 16) * channels * 2
                    + if stages >= 3 {
                        ((sw - 2) * (sh - 2) + 16) * channels * 2
                    } else {
                        0
                    }
                    + 256 * if half { 2 } else { 4 }
                    + if stem {
                        ((sw + 4) * (sh + 4) + 16 + groups * 256) * 2
                    } else {
                        0
                    };
                if shared > gpu.limits.max_compute_shared_memory_size {
                    return Err(error("fused tile exceeds shared memory limit"));
                }
                let mut values = specializations[last].clone();
                values.resize(51, 0);
                values[26] = groups;
                values[32..37].copy_from_slice(&[
                    channels,
                    stages,
                    tile[0],
                    tile[1],
                    packed.metadata[packed.metadata[packed.headers[first] as usize + 13] as usize],
                ]);
                let mut relus = 0;
                for (s, &header) in packed.headers[first..=last].iter().enumerate() {
                    let h = header as usize;
                    values[if s < 3 { 37 + s } else { 49 }] = if stem && s == 0 {
                        packed.metadata[h + 3]
                    } else {
                        packed.metadata[packed.metadata[h + 13] as usize + 5]
                    };
                    values[if s < 3 { 40 + s } else { 50 }] = packed.metadata[h + 5];
                    if matches!(model.layers[first + s].activation, Activation::Relu) {
                        relus |= 1 << s;
                    }
                }
                values[43] = relus;
                let h = packed.headers[first] as usize;
                values[44..49].copy_from_slice(&[
                    u32::from(stem),
                    packed.metadata[h + 2],
                    packed.metadata[h + 10],
                    packed.metadata[h + 11],
                    packed.metadata[h + 9],
                ]);
                let pipeline = out.compile(
                    if half {
                        include_bytes!("vk_coopmat/fused_half.spv")
                    } else {
                        include_bytes!("vk_coopmat/fused.spv")
                    },
                    &values,
                )?;
                out.fused.push(FusedPipeline {
                    first,
                    last,
                    pipeline,
                    tile,
                });
            }
        }
        Ok(out)
    }
    fn compile(&self, spirv: &[u8], values: &[u32]) -> Result<vk::Pipeline> {
        unsafe {
            let words: Vec<u32> = spirv
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                .collect();
            let module = self
                .gpu
                .device
                .create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&words), None)
                .map_err(error)?;
            let entries: Vec<_> = (0..values.len() as u32)
                .map(|i| {
                    vk::SpecializationMapEntry::default()
                        .constant_id(i)
                        .offset(i * 4)
                        .size(4)
                })
                .collect();
            let spec = vk::SpecializationInfo::default()
                .map_entries(&entries)
                .data(bytes(values));
            let mut stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            if !values.is_empty() {
                stage = stage.specialization_info(&spec);
            }
            let flags = if self.gpu.capture {
                vk::PipelineCreateFlags::CAPTURE_STATISTICS_KHR
                    | vk::PipelineCreateFlags::CAPTURE_INTERNAL_REPRESENTATIONS_KHR
            } else {
                vk::PipelineCreateFlags::empty()
            };
            let result = self.gpu.device.create_compute_pipelines(
                vk::PipelineCache::null(),
                &[vk::ComputePipelineCreateInfo::default()
                    .flags(flags)
                    .stage(stage)
                    .layout(self.layout)],
                None,
            );
            self.gpu.device.destroy_shader_module(module, None);
            match result {
                Ok(p) => Ok(p[0]),
                Err((partial, e)) => {
                    for p in partial {
                        self.gpu.device.destroy_pipeline(p, None);
                    }
                    Err(error(e))
                }
            }
        }
    }
}

struct PackedModel {
    weights: Vec<u16>,
    metadata: Vec<u32>,
    headers: Vec<u32>,
    channels: u32,
    head_weights: Vec<f32>,
    fusions: Vec<(usize, usize)>,
    coordinates: [u32; 2],
}

fn pack_model(model: &SrModel, shape: [u32; 4]) -> PackedModel {
    let align = shape[0].max(shape[2]).max(16);
    let fusions = fusion_runs(model, shape);
    let (offsets, channels) = arena_layout(model, align, &fusions);
    let mut weights = Vec::new();
    let mut metadata = Vec::new();
    let mut headers = Vec::new();
    let mut head_weights = vec![0.0; 4096];
    for layer in &model.layers {
        let header = metadata.len();
        metadata.resize(header + HEADER, 0);
        headers.push(header as u32);
        let mut descriptors = Vec::new();
        for input in &layer.inputs {
            for c in 0..model.tensors[input.tensor] {
                for ky in 0..input.kernel {
                    for kx in 0..input.kernel {
                        descriptors.extend_from_slice(&[
                            offsets[input.tensor] + c as u32,
                            (kx as i32 - input.kernel as i32 / 2) as u32,
                            (ky as i32 - input.kernel as i32 / 2) as u32,
                            match input.transform {
                                InputTransform::Identity => 0,
                                InputTransform::Relu => 1,
                                InputTransform::NegRelu => 2,
                            },
                        ]);
                    }
                }
            }
        }
        let real_k = descriptors.len() / 4;
        let k = (real_k as u32).div_ceil(shape[2]) * shape[2];
        while descriptors.len() < k as usize * 4 {
            descriptors.extend_from_slice(&[u32::MAX, 0, 0, 0]);
        }
        let desc_offset = metadata.len() as u32;
        metadata.extend(descriptors);
        let out_padded = (layer.out_channels as u32).div_ceil(align) * align;
        let weight_offset = weights.len() as u32;
        let variants = if layer.border.is_some() { 10 } else { 1 };
        for variant in 0..variants {
            let start = weights.len();
            weights.resize(start + (out_padded * k) as usize, 0);
            for o in 0..layer.out_channels {
                let mut column = 0;
                for (i, input) in layer.inputs.iter().enumerate() {
                    let source = if variant == 0 {
                        &input.weights
                    } else {
                        &layer.border.as_ref().unwrap()[variant - 1].inputs[i]
                    };
                    let count = model.tensors[input.tensor] * input.kernel * input.kernel;
                    for j in 0..count {
                        weights[start + o * k as usize + column + j] =
                            f32_to_f16(source[o * count + j]);
                    }
                    column += count;
                }
            }
        }
        let bias_offset = metadata.len() as u32;
        for variant in 0..variants {
            let bias = if variant == 0 {
                &layer.bias
            } else {
                &layer.border.as_ref().unwrap()[variant - 1].bias
            };
            metadata.extend(bias.iter().map(|x| x.to_bits()));
        }
        let alpha_offset = metadata.len() as u32;
        let activation = match &layer.activation {
            Activation::None => 0,
            Activation::Relu => 1,
            Activation::Silu => 2,
            Activation::Prelu(alpha) => {
                metadata.extend(alpha.iter().map(|x| x.to_bits()));
                3
            }
        };
        let head = if layer.output == model.head.tensor {
            1 | (u32::from(model.head.order == SubpixelOrder::ColumnMajor) * 2)
                | (u32::from(model.head.base == HeadBase::Bilinear) * 4)
                | (u32::from(model.head.clamp) * 8)
        } else {
            0
        };
        metadata[header..header + 13].copy_from_slice(&[
            offsets[layer.output],
            layer.out_channels as u32,
            k,
            weight_offset,
            desc_offset,
            bias_offset,
            layer.residual.map_or(u32::MAX, |t| offsets[t]),
            activation,
            alpha_offset,
            u32::from(layer.border.is_some()),
            out_padded * k,
            layer.out_channels as u32,
            head,
        ]);
        // Per-tap channel matrices support direct cooperative loads from
        // channel-blocked feature buffers. Keep the flattened OIHW copy above
        // for the small scalar border correction only.
        let tap_offset = metadata.len() as u32;
        let mut taps = 0u32;
        let mut head_base = 0;
        for input in &layer.inputs {
            let in_channels = model.tensors[input.tensor];
            let in_padded = (in_channels as u32).div_ceil(align) * align;
            for ky in 0..input.kernel {
                for kx in 0..input.kernel {
                    let start = weights.len();
                    weights.resize(start + (out_padded * in_padded) as usize, 0);
                    for o in 0..layer.out_channels {
                        for c in 0..in_channels {
                            weights[start + o * in_padded as usize + c] = f32_to_f16(
                                input.weights[((o * in_channels + c) * input.kernel + ky)
                                    * input.kernel
                                    + kx],
                            );
                        }
                    }
                    metadata.extend_from_slice(&[
                        offsets[input.tensor],
                        (kx as i32 - input.kernel as i32 / 2) as u32,
                        (ky as i32 - input.kernel as i32 / 2) as u32,
                        match input.transform {
                            InputTransform::Identity => 0,
                            InputTransform::Relu => 1,
                            InputTransform::NegRelu => 2,
                        },
                        in_padded,
                        start as u32,
                        in_channels as u32,
                        head_base as u32 | (u32::from(input.tensor == 0) << 31),
                    ]);
                    taps += 1;
                }
            }
            if layer.output == model.head.tensor {
                let count = in_channels * input.kernel * input.kernel;
                for i in 0..count.min(1024usize.saturating_sub(head_base)) {
                    for o in 0..4 {
                        head_weights[(head_base + i) * 4 + o] = input.weights[o * count + i];
                    }
                }
                head_base += count;
            }
        }
        metadata[header + 13] = tap_offset;
        metadata[header + 14] = taps;
        metadata[header + 15] = out_padded;
    }
    let coordinates = [metadata.len() as u32, weights.len() as u32];
    for i in 0..shape[0] * shape[1] {
        metadata.push((i as f32).to_bits());
        weights.push(f32_to_f16(i as f32));
    }
    PackedModel {
        weights,
        metadata,
        headers,
        channels,
        head_weights,
        fusions,
        coordinates,
    }
}

/// Reuse dead tensors while preserving every DAG input/residual through its
/// last consumer. A dispatch never aliases its output with a live input.
fn arena_layout(model: &SrModel, align: u32, fusions: &[(usize, usize)]) -> (Vec<u32>, u32) {
    let mut last = vec![0usize; model.tensors.len()];
    for (index, layer) in model.layers.iter().enumerate() {
        for input in &layer.inputs {
            last[input.tensor] = index;
        }
        if let Some(t) = layer.residual {
            last[t] = index;
        }
    }
    for &(first, end) in fusions {
        let input = model.layers[first].inputs[0].tensor;
        last[input] = last[input].max(end);
    }
    let padded = |c: usize| (c as u32).div_ceil(align) * align;
    let mut offsets = vec![0; model.tensors.len()];
    let mut channels = padded(model.tensors[0]);
    // (offset, capacity, last live layer); unused slots remain available.
    let mut slots = vec![(0, channels, last[0])];
    for (index, layer) in model.layers.iter().enumerate() {
        if fusions
            .iter()
            .any(|&(first, last)| index >= first && index < last)
        {
            continue;
        }
        if layer.output == model.head.tensor {
            continue;
        }
        let count = padded(layer.out_channels);
        let candidate = slots
            .iter()
            .enumerate()
            .filter(|(_, (_, capacity, end))| *end < index && *capacity >= count)
            .min_by_key(|(_, (_, capacity, _))| *capacity)
            .map(|(i, _)| i);
        if let Some(slot) = candidate {
            offsets[layer.output] = slots[slot].0;
            slots[slot].2 = last[layer.output];
        } else {
            offsets[layer.output] = channels;
            slots.push((channels, count, last[layer.output]));
            channels += count;
        }
    }
    (offsets, channels)
}

struct Prepared {
    model: Arc<SrModel>,
    weights: Buffer,
    metadata: Buffer,
    head_weights: Buffer,
    packed: PackedModel,
    pipelines: Pipelines,
    tile: [u32; 2],
    halos: Vec<usize>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Params {
    size: [u32; 4],
    origin: [i32; 4],
    core: [u32; 4],
    norm: [f32; 4],
    active: [u32; 4],
}

struct Pool {
    gpu: Arc<Gpu>,
    width: u32,
    height: u32,
    tw: u32,
    th: u32,
    cw: u32,
    ch: u32,
    halo: u32,
    input: SharedImage,
    output: SharedImage,
    _arena: Buffer,
    descriptors: vk::DescriptorPool,
    set: vk::DescriptorSet,
}

fn cache_tile(
    width: u32,
    height: u32,
    halo: u32,
    channels: u32,
    guard: u32,
    guard_y: u32,
    budget: u64,
) -> Option<(u32, u32)> {
    let mut best = None;
    for gx in 1..=width.min(64) {
        for gy in 1..=height.min(64) {
            let cw = width.div_ceil(gx);
            let ch = height.div_ceil(gy);
            let area = u64::from(cw + 2 * halo + guard) * u64::from(ch + 2 * halo + guard_y);
            if area * u64::from(channels) * 2 > budget {
                continue;
            }
            let count = u64::from(width.div_ceil(cw)) * u64::from(height.div_ceil(ch));
            let cost = (area + 2048) * count;
            if best.is_none_or(|(score, _, _)| cost < score) {
                best = Some((cost, cw, ch));
            }
        }
    }
    best.map(|(_, w, h)| (w, h))
}
impl Drop for Pool {
    fn drop(&mut self) {
        unsafe {
            self.gpu
                .device
                .destroy_descriptor_pool(self.descriptors, None);
        }
    }
}
impl Pool {
    fn new(
        gpu: &Arc<Gpu>,
        d3d: &ID3D11Device,
        prepared: &Prepared,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let halo = *prepared.halos.iter().max().unwrap_or(&0) as u32;
        let budget = ARENA_LIMIT.min(u64::from(gpu.limits.max_storage_buffer_range));
        // Right guard for the final partial subgroup's contiguous matrix load.
        // Only valid output columns contribute; every loaded address still
        // belongs to its allocated feature plane, including the final row.
        let guard = (gpu.shape[1]
            * prepared
                .pipelines
                .layer_groups
                .iter()
                .zip(&prepared.pipelines.strips)
                .map(|(&g, &s)| g * s)
                .max()
                .unwrap_or(1))
        .max(
            prepared
                .pipelines
                .fused
                .iter()
                .map(|f| f.tile[0] + 32)
                .max()
                .unwrap_or(0),
        );
        let guard_y = prepared
            .pipelines
            .fused
            .iter()
            .map(|f| f.tile[1] + 8)
            .max()
            .unwrap_or(0);
        let (mut cw, mut ch) = if prepared.tile[0] > 0 && prepared.tile[1] > 0 {
            (width.min(prepared.tile[0]), height.min(prepared.tile[1]))
        } else {
            cache_tile(
                width,
                height,
                halo,
                prepared.packed.channels,
                guard,
                guard_y,
                budget,
            )
            .unwrap_or((width.min(1024), height.min(256)))
        };
        if let Some(mib) = std::env::var("ERIKA_VK_BAND_MIB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|&v| v > 0)
        {
            let row_bytes =
                u64::from(width + 2 * halo + guard) * u64::from(prepared.packed.channels) * 2;
            cw = width;
            ch = (mib.saturating_mul(1024 * 1024).min(budget) / row_bytes)
                .saturating_sub(u64::from(2 * halo + guard_y))
                .max(1)
                .min(u64::from(height)) as u32;
        }
        loop {
            let size = u64::from(cw + 2 * halo + guard)
                * u64::from(ch + 2 * halo + guard_y)
                * u64::from(prepared.packed.channels)
                * 2;
            if size <= budget {
                break;
            }
            if cw >= ch && cw > 1 {
                cw = cw.div_ceil(2);
            } else if ch > 1 {
                ch = ch.div_ceil(2);
            } else {
                return Err(error("network halo exceeds the intermediate memory budget"));
            }
        }
        let tw = cw + 2 * halo + guard;
        let th = ch + 2 * halo + guard_y;
        let arena = Buffer::new(
            gpu,
            u64::from(tw) * u64::from(th) * u64::from(prepared.packed.channels) * 2,
            None,
        )?;
        let input = SharedImage::new(gpu, d3d, width, height, false)?;
        let output = SharedImage::new(gpu, d3d, width * 2, height * 2, true)?;
        let mut pool = Self {
            gpu: gpu.clone(),
            width,
            height,
            tw,
            th,
            cw,
            ch,
            halo,
            input,
            output,
            _arena: arena,
            descriptors: vk::DescriptorPool::null(),
            set: vk::DescriptorSet::null(),
        };
        unsafe {
            let sizes = [
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(3),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::STORAGE_IMAGE)
                    .descriptor_count(2),
                vk::DescriptorPoolSize::default()
                    .ty(vk::DescriptorType::UNIFORM_BUFFER)
                    .descriptor_count(1),
            ];
            pool.descriptors = gpu
                .device
                .create_descriptor_pool(
                    &vk::DescriptorPoolCreateInfo::default()
                        .max_sets(1)
                        .pool_sizes(&sizes),
                    None,
                )
                .map_err(error)?;
            pool.set = gpu
                .device
                .allocate_descriptor_sets(
                    &vk::DescriptorSetAllocateInfo::default()
                        .descriptor_pool(pool.descriptors)
                        .set_layouts(&[prepared.pipelines.set_layout]),
                )
                .map_err(error)?[0];
            for (i, buffer) in [&pool._arena, &prepared.weights, &prepared.metadata]
                .iter()
                .enumerate()
            {
                let info = [vk::DescriptorBufferInfo::default()
                    .buffer(buffer.buffer)
                    .range(buffer.size)];
                gpu.device.update_descriptor_sets(
                    &[vk::WriteDescriptorSet::default()
                        .dst_set(pool.set)
                        .dst_binding(i as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                        .buffer_info(&info)],
                    &[],
                );
            }
            for (i, image) in [&pool.input, &pool.output].iter().enumerate() {
                let info = [vk::DescriptorImageInfo::default()
                    .image_view(image.view)
                    .image_layout(vk::ImageLayout::GENERAL)];
                gpu.device.update_descriptor_sets(
                    &[vk::WriteDescriptorSet::default()
                        .dst_set(pool.set)
                        .dst_binding(3 + i as u32)
                        .descriptor_type(vk::DescriptorType::STORAGE_IMAGE)
                        .image_info(&info)],
                    &[],
                );
            }
            let info = [vk::DescriptorBufferInfo::default()
                .buffer(prepared.head_weights.buffer)
                .range(16384)];
            gpu.device.update_descriptor_sets(
                &[vk::WriteDescriptorSet::default()
                    .dst_set(pool.set)
                    .dst_binding(5)
                    .descriptor_type(vk::DescriptorType::UNIFORM_BUFFER)
                    .buffer_info(&info)],
                &[],
            );
        }
        Ok(pool)
    }
}

struct Flight {
    command: vk::CommandBuffer,
    query: vk::QueryPool,
    complete: u64,
}

pub(crate) struct VkCoopMatUpscaler {
    gpu: Arc<Gpu>,
    d3d: ID3D11Device,
    fence: ID3D11Fence,
    semaphore: vk::Semaphore,
    commands: vk::CommandPool,
    copy: ID3D11ComputeShader,
    prepared: Option<Prepared>,
    pool: Option<Pool>,
    flights: VecDeque<Flight>,
    recycled: Vec<Flight>,
    value: u64,
    last_gpu: Option<Duration>,
    half_accumulation: bool,
    groups: u32,
    tile: [u32; 2],
    strips: u32,
    #[cfg(test)]
    profile: bool,
}

impl Drop for VkCoopMatUpscaler {
    fn drop(&mut self) {
        unsafe {
            let _ = self.gpu.device.device_wait_idle();
            for f in self.flights.iter().chain(&self.recycled) {
                self.gpu.device.destroy_query_pool(f.query, None);
            }
            self.gpu.device.destroy_command_pool(self.commands, None);
            self.gpu.device.destroy_semaphore(self.semaphore, None);
        }
    }
}

impl VkCoopMatUpscaler {
    /// Dynamically opens Vulkan on the D3D device's adapter and imports a shared fence.
    pub(crate) fn new(device: &ID3D11Device) -> Result<Self> {
        unsafe {
            let d5: ID3D11Device5 = device
                .cast()
                .map_err(|e| error(format!("ID3D11Device5 required: {e}")))?;
            let context = device.GetImmediateContext().map_err(error)?;
            let _: ID3D11DeviceContext4 = context
                .cast()
                .map_err(|e| error(format!("ID3D11DeviceContext4 required: {e}")))?;
            let gpu = Gpu::new(device)?;
            let mut fence = None;
            d5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut fence)
                .map_err(error)?;
            let fence: ID3D11Fence = fence.ok_or_else(|| error("CreateFence returned null"))?;
            let handle = Handle(
                fence
                    .CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null())
                    .map_err(error)?,
            );
            let copy = copy_shader(device)?;
            let half_accumulation = gpu.half_supported
                && std::env::var_os("ERIKA_VK_FP16_ACCUMULATION").is_some_and(|v| v == "1");
            let mut out = Self {
                gpu,
                d3d: device.clone(),
                fence,
                semaphore: vk::Semaphore::null(),
                commands: vk::CommandPool::null(),
                copy,
                prepared: None,
                pool: None,
                flights: VecDeque::new(),
                recycled: Vec::new(),
                value: 0,
                last_gpu: None,
                half_accumulation,
                groups: 0,
                tile: [0, 0],
                strips: 0,
                #[cfg(test)]
                profile: false,
            };
            let mut ty =
                vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE);
            out.semaphore = out
                .gpu
                .device
                .create_semaphore(&vk::SemaphoreCreateInfo::default().push_next(&mut ty), None)
                .map_err(error)?;
            ash::khr::external_semaphore_win32::Device::new(&out.gpu.instance.0, &out.gpu.device)
                .import_semaphore_win32_handle(
                    &vk::ImportSemaphoreWin32HandleInfoKHR::default()
                        .semaphore(out.semaphore)
                        .handle_type(vk::ExternalSemaphoreHandleTypeFlags::D3D12_FENCE)
                        .handle(handle.0.0 as isize),
                )
                .map_err(error)?;
            out.commands = out
                .gpu
                .device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default()
                        .queue_family_index(out.gpu.family)
                        .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                    None,
                )
                .map_err(error)?;
            // Fail at construction, before the renderer selects this backend,
            // if the adapter cannot share either of the required typed images.
            let _input_probe = SharedImage::new(&out.gpu, device, 1, 1, false)?;
            let _output_probe = SharedImage::new(&out.gpu, device, 1, 1, true)?;
            Ok(out)
        }
    }

    fn execution_settings(&self, model: &SrModel) -> (u32, u32) {
        let channels = model.tensors.iter().copied().max().unwrap_or(0);
        let (mut groups, strips) = if element_epilogues(&self.gpu, model) {
            if self.half_accumulation {
                (1, if channels == 16 { 4 } else { 2 })
            } else if channels < 16 {
                (2, 1)
            } else {
                (4, 1)
            }
        } else {
            (1, 2)
        };
        if self.groups != 0 {
            groups = self.groups;
        } else if model
            .layers
            .iter()
            .any(|l| layer_tile(&self.gpu, model, l, groups, self.half_accumulation).is_none())
        {
            groups = 1;
        }
        (
            groups,
            if self.strips == 0 {
                strips
            } else {
                self.strips
            },
        )
    }

    pub(crate) fn supports(&self, model: &SrModel) -> bool {
        if model.validate().is_err()
            || model.tensors.len() > 1024
            || model.tensors.iter().sum::<usize>() > 65536
        {
            return false;
        }
        let (groups, _) = self.execution_settings(model);
        model.layers.iter().all(|layer| {
            layer.inputs.iter().all(|i| {
                (1..=9).contains(&i.kernel) && i.kernel % 2 == 1 && i.tensor != model.head.tensor
            }) && layer.residual != Some(model.head.tensor)
                && layer.border.as_ref().is_none_or(|v| {
                    v[4].bias == layer.bias
                        && v[4]
                            .inputs
                            .iter()
                            .zip(&layer.inputs)
                            .all(|(w, i)| w == &i.weights)
                })
                && layer_tile(&self.gpu, model, layer, groups, self.half_accumulation).is_some()
        }) && model
            .layers
            .last()
            .is_some_and(|l| l.output == model.head.tensor)
    }

    pub(crate) fn prepare(&mut self, model: Arc<SrModel>) -> Result<()> {
        if !self.supports(&model) {
            return Err(error("unsupported or malformed SR graph"));
        }
        self.idle()?;
        let (groups, strips) = self.execution_settings(&model);
        let packed = pack_model(&model, self.gpu.shape);
        for size in [packed.weights.len() * 2, packed.metadata.len() * 4] {
            if size > self.gpu.limits.max_storage_buffer_range as usize {
                return Err(error("model exceeds maxStorageBufferRange"));
            }
        }
        let weights = Buffer::new(
            &self.gpu,
            (packed.weights.len() * 2) as u64,
            Some(bytes(&packed.weights)),
        )?;
        let metadata = Buffer::new(
            &self.gpu,
            (packed.metadata.len() * 4) as u64,
            Some(bytes(&packed.metadata)),
        )?;
        let half_head = half_spatial_head(&self.gpu, &model);
        let head_weights = if half_head {
            let coefficients = pack_half_head_weights(&packed.head_weights);
            Buffer::new(&self.gpu, 16384, Some(bytes(&coefficients)))?
        } else {
            Buffer::new(&self.gpu, 16384, Some(bytes(&packed.head_weights)))?
        };
        if model
            .layers
            .iter()
            .any(|l| layer_tile(&self.gpu, &model, l, groups, self.half_accumulation).is_none())
        {
            return Err(error("matrix tile exceeds shared memory limit"));
        }
        let pipelines = Pipelines::new(
            &self.gpu,
            self.half_accumulation,
            half_head,
            &model,
            &packed,
            groups,
            strips,
        )?;
        self.pool = None;
        let halos = model.tensor_halos();
        self.prepared = Some(Prepared {
            model,
            weights,
            metadata,
            head_weights,
            packed,
            pipelines,
            tile: self.tile,
            halos,
        });
        self.last_gpu = None;
        Ok(())
    }

    fn idle(&mut self) -> Result<()> {
        unsafe {
            self.gpu.device.device_wait_idle().map_err(error)?;
        }
        self.collect()?;
        Ok(())
    }

    fn collect(&mut self) -> Result<()> {
        unsafe {
            let complete = self
                .gpu
                .device
                .get_semaphore_counter_value(self.semaphore)
                .map_err(error)?;
            while self.flights.front().is_some_and(|f| f.complete <= complete) {
                let flight = self.flights.pop_front().unwrap();
                if self.gpu.timestamp_bits > 0 {
                    let mut results = [[0u64; 2]; 2];
                    // Availability words avoid WAIT_BIT, including on drivers where the
                    // externally shared timeline becomes visible before query readback.
                    match self.gpu.device.get_query_pool_results(
                        flight.query,
                        0,
                        &mut results,
                        vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WITH_AVAILABILITY,
                    ) {
                        Ok(()) if results[0][1] != 0 && results[1][1] != 0 => {
                            let mask = u64::MAX >> (64 - self.gpu.timestamp_bits);
                            let ticks = results[1][0].wrapping_sub(results[0][0]) & mask;
                            self.last_gpu = Some(Duration::from_secs_f64(
                                ticks as f64 * f64::from(self.gpu.limits.timestamp_period) * 1e-9,
                            ));
                        }
                        Ok(()) | Err(vk::Result::NOT_READY) => {}
                        Err(e) => {
                            self.recycled.push(flight);
                            return Err(error(e));
                        }
                    }
                }
                self.recycled.push(flight);
                #[cfg(test)]
                if self.profile {
                    let flight = self.recycled.last().unwrap();
                    let mut stamps = vec![[0u64; 2]; (self.query_count() - 2) as usize];
                    if self
                        .gpu
                        .device
                        .get_query_pool_results(
                            flight.query,
                            2,
                            &mut stamps,
                            vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WITH_AVAILABILITY,
                        )
                        .is_ok()
                    {
                        let prepared = self.prepared.as_ref().unwrap();
                        let layers = prepared.model.layers.len();
                        let mut totals = vec![0.0; layers];
                        let mut convolutions = vec![0.0; layers];
                        for (i, pair) in stamps.chunks_exact(3).enumerate() {
                            totals[i % layers] += pair[2][0].wrapping_sub(pair[0][0]) as f64
                                * f64::from(self.gpu.limits.timestamp_period)
                                * 1e-6;
                            convolutions[i % layers] += pair[1][0].wrapping_sub(pair[0][0]) as f64
                                * f64::from(self.gpu.limits.timestamp_period)
                                * 1e-6;
                        }
                        let pool = self.pool.as_ref().unwrap();
                        for (i, ms) in totals.into_iter().enumerate() {
                            let layer = &prepared.model.layers[i];
                            let mut inputs: Vec<_> =
                                layer.inputs.iter().map(|v| v.tensor).collect();
                            if let Some(t) = layer.residual {
                                inputs.push(t);
                            }
                            inputs.sort_unstable();
                            inputs.dedup();
                            let head = layer.output == prepared.model.head.tensor;
                            let vector = head && prepared.pipelines.vector_head;
                            let input_bytes: usize = inputs
                                .iter()
                                .map(|&t| {
                                    if t == 0 && !vector {
                                        4
                                    } else if vector {
                                        prepared.model.tensors[t] * 2
                                    } else {
                                        prepared.model.tensors[t].div_ceil(16) * 32
                                    }
                                })
                                .sum();
                            let output = if layer.output == prepared.model.head.tensor {
                                4
                            } else {
                                layer.out_channels.div_ceil(16) * 16
                            };
                            let pixels = f64::from(pool.width) * f64::from(pool.height);
                            let traffic = (input_bytes + output * 2) as f64 * pixels;
                            let [m, n, k, _] = self.gpu.shape;
                            let align = m.max(k).max(16);
                            let ksteps: u32 = layer
                                .inputs
                                .iter()
                                .map(|v| {
                                    if v.tensor == 0 {
                                        (v.kernel as u32 * v.kernel as u32).div_ceil(k)
                                    } else {
                                        (prepared.model.tensors[v.tensor] as u32).div_ceil(align)
                                            * align
                                            / k
                                            * (v.kernel * v.kernel) as u32
                                    }
                                })
                                .sum();
                            let mut matrices = 0u64;
                            if !vector
                                && !prepared
                                    .pipelines
                                    .fused
                                    .iter()
                                    .any(|f| f.first <= i && i <= f.last)
                            {
                                let span = n
                                    * prepared.pipelines.layer_groups[i]
                                    * prepared.pipelines.strips[i];
                                let halo = prepared.halos[layer.output] as u32;
                                for y in (0..pool.height).step_by(pool.ch as usize) {
                                    for x in (0..pool.width).step_by(pool.cw as usize) {
                                        matrices += u64::from(
                                            (pool.cw.min(pool.width - x) + 2 * halo).div_ceil(span)
                                                * span
                                                / n,
                                        ) * u64::from(
                                            pool.ch.min(pool.height - y) + 2 * halo,
                                        ) * u64::from(
                                            (layer.out_channels as u32).div_ceil(align) * align / m,
                                        ) * u64::from(ksteps);
                                    }
                                }
                            }
                            let macs: usize = layer
                                .inputs
                                .iter()
                                .map(|v| {
                                    v.kernel
                                        * v.kernel
                                        * prepared.model.tensors[v.tensor]
                                        * layer.out_channels
                                })
                                .sum();
                            eprintln!(
                                "LAYER {} {} fp{} ms={ms:.6} conv_ms={:.6} correction_ms={:.6} min_MB={:.3} effective_GBs={:.3} useful_TFLOPS={:.3} estimated_MMA_Gs={:.4}",
                                prepared.model.name,
                                i,
                                if self.half_accumulation { 16 } else { 32 },
                                convolutions[i],
                                ms - convolutions[i],
                                traffic / 1e6,
                                traffic / ms / 1e6,
                                2.0 * macs as f64 * pixels / ms / 1e9,
                                matrices as f64 / convolutions[i] / 1e6
                            );
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn last_gpu_duration(&self) -> Option<Duration> {
        self.last_gpu
    }

    fn query_count(&self) -> u32 {
        #[cfg(test)]
        if self.profile {
            let tiles = self
                .pool
                .as_ref()
                .map_or(1, |p| p.width.div_ceil(p.cw) * p.height.div_ceil(p.ch));
            return 2 + 3 * tiles * self.prepared.as_ref().unwrap().model.layers.len() as u32;
        }
        2
    }

    /// Enqueues the D3D -> Vulkan -> D3D handoff. The returned raw-domain R16F
    /// SRV remains valid until the next encode/prepare. Size changes may wait
    /// for outstanding Vulkan work; ordinary frames only poll completion.
    pub(crate) fn encode(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        luma: &ID3D11ShaderResourceView,
        width: u32,
        height: u32,
        range: LumaRange,
    ) -> Result<ID3D11ShaderResourceView> {
        if device.as_raw() != self.d3d.as_raw() {
            return Err(error("D3D device changed; recreate the sidecar"));
        }
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return Err(error("input extent must be 1..8192"));
        }
        let c4: ID3D11DeviceContext4 = context.cast().map_err(error)?;
        unsafe {
            if context.GetType() != D3D11_DEVICE_CONTEXT_IMMEDIATE
                || context.GetDevice().map_err(error)?.as_raw() != device.as_raw()
            {
                return Err(error("encode requires this device's immediate context"));
            }
        }
        self.collect()?;
        if self
            .pool
            .as_ref()
            .is_none_or(|p| p.width != width || p.height != height)
        {
            self.idle()?;
            let prepared = self
                .prepared
                .as_ref()
                .ok_or_else(|| error("prepare a model before encode"))?;
            self.pool = Some(Pool::new(&self.gpu, device, prepared, width, height)?);
        }
        let mut flight = if let Some(flight) = self.recycled.pop() {
            flight
        } else {
            unsafe {
                let command = self
                    .gpu
                    .device
                    .allocate_command_buffers(
                        &vk::CommandBufferAllocateInfo::default()
                            .command_pool(self.commands)
                            .level(vk::CommandBufferLevel::PRIMARY)
                            .command_buffer_count(1),
                    )
                    .map_err(error)?[0];
                let query_result = self.gpu.device.create_query_pool(
                    &vk::QueryPoolCreateInfo::default()
                        .query_type(vk::QueryType::TIMESTAMP)
                        .query_count(self.query_count()),
                    None,
                );
                let query = match query_result {
                    Ok(q) => q,
                    Err(e) => {
                        self.gpu
                            .device
                            .free_command_buffers(self.commands, &[command]);
                        return Err(error(e));
                    }
                };
                Flight {
                    command,
                    query,
                    complete: 0,
                }
            }
        };
        let prepared = self.prepared.as_ref().unwrap();
        let pool = self.pool.as_ref().unwrap();
        unsafe {
            if let Err(e) = self.record(&flight, prepared, pool, range) {
                self.recycled.push(flight);
                return Err(e);
            }
            // All fallible command recording/resource creation precedes the D3D
            // signal, so a preparation error never queues an unsatisfiable Wait.
            context.CSSetShader(&self.copy, None);
            context.CSSetShaderResources(0, Some(&[Some(luma.clone())]));
            context.CSSetUnorderedAccessViews(
                0,
                1,
                Some([Some(pool.input.uav.clone())].as_ptr()),
                None,
            );
            context.Dispatch(width.div_ceil(8), height.div_ceil(8), 1);
            context.CSSetShaderResources(0, Some(&[None]));
            context.CSSetUnorderedAccessViews(0, 1, Some([None].as_ptr()), None);
            context.CSSetShader(None, None);
            let done = self
                .value
                .checked_add(2)
                .ok_or_else(|| error("fence value exhausted"))?;
            let ready = done - 1;
            if let Err(e) = c4.Signal(&self.fence, ready) {
                self.recycled.push(flight);
                return Err(error(e));
            }
            context.Flush(); // Submit the D3D signal before Vulkan waits on it.
            let waits = [self.semaphore];
            let values = [ready];
            let signals = [done];
            let stages = [vk::PipelineStageFlags::ALL_COMMANDS];
            let cmds = [flight.command];
            let mut timeline = vk::TimelineSemaphoreSubmitInfo::default()
                .wait_semaphore_values(&values)
                .signal_semaphore_values(&signals);
            let submit = vk::SubmitInfo::default()
                .wait_semaphores(&waits)
                .wait_dst_stage_mask(&stages)
                .command_buffers(&cmds)
                .signal_semaphores(&waits)
                .push_next(&mut timeline);
            let submitted =
                self.gpu
                    .device
                    .queue_submit(self.gpu.queue, &[submit], vk::Fence::null());
            self.value = done;
            if let Err(e) = submitted {
                self.recycled.push(flight);
                return Err(error(e));
            }
            flight.complete = done;
            self.flights.push_back(flight);
            c4.Wait(&self.fence, done).map_err(error)?;
        }
        Ok(pool.output.srv.clone())
    }

    unsafe fn record(
        &self,
        flight: &Flight,
        prepared: &Prepared,
        pool: &Pool,
        range: LumaRange,
    ) -> Result<()> {
        unsafe {
            let d = &self.gpu.device;
            let cmd = flight.command;
            d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                .map_err(error)?;
            d.begin_command_buffer(
                cmd,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )
            .map_err(error)?;
            if self.gpu.timestamp_bits > 0 {
                d.cmd_reset_query_pool(cmd, flight.query, 0, self.query_count());
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, flight.query, 0);
            }
            let acquire: Vec<_> = [&pool.input, &pool.output]
                .iter()
                .map(|i| {
                    vk::ImageMemoryBarrier::default()
                        .image(i.image)
                        .old_layout(vk::ImageLayout::GENERAL)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                        .dst_queue_family_index(self.gpu.family)
                        .dst_access_mask(
                            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                        )
                        .subresource_range(image_range())
                })
                .collect();
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &acquire,
            );
            d.cmd_bind_descriptor_sets(
                cmd,
                vk::PipelineBindPoint::COMPUTE,
                prepared.pipelines.layout,
                0,
                &[pool.set],
                &[],
            );
            let (offset, scale) = input_normalization(&prepared.model, range);
            for y in (0..pool.height).step_by(pool.ch as usize) {
                for x in (0..pool.width).step_by(pool.cw as usize) {
                    let mut p = Params {
                        size: [pool.width, pool.height, pool.tw, pool.th],
                        origin: [
                            x as i32 - pool.halo as i32,
                            y as i32 - pool.halo as i32,
                            x as i32,
                            y as i32,
                        ],
                        core: [
                            pool.cw.min(pool.width - x),
                            pool.ch.min(pool.height - y),
                            self.gpu.shape[0].max(self.gpu.shape[2]).max(16),
                            u32::from(prepared.model.padding == Padding::Clamp),
                        ],
                        norm: [offset, scale, 0.0, 0.0],
                        active: [0, 0, pool.tw, pool.th],
                    };
                    // Also orders the previous tile's reads before the arena is reused.
                    compute_barrier(d, cmd);
                    d.cmd_push_constants(
                        cmd,
                        prepared.pipelines.layout,
                        vk::ShaderStageFlags::COMPUTE,
                        0,
                        bytes(&[p]),
                    );
                    d.cmd_bind_pipeline(
                        cmd,
                        vk::PipelineBindPoint::COMPUTE,
                        prepared.pipelines.input,
                    );
                    d.cmd_dispatch(cmd, (pool.tw * pool.th * p.core[2]).div_ceil(128), 1, 1);
                    compute_barrier(d, cmd);
                    for (index, (&header, layer)) in prepared
                        .packed
                        .headers
                        .iter()
                        .zip(&prepared.model.layers)
                        .enumerate()
                    {
                        #[cfg(test)]
                        let profile_query = 2
                            + ((y / pool.ch * pool.width.div_ceil(pool.cw) + x / pool.cw)
                                * prepared.model.layers.len() as u32
                                + index as u32)
                                * 3;
                        #[cfg(test)]
                        if self.profile {
                            d.cmd_write_timestamp(
                                cmd,
                                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                                flight.query,
                                profile_query,
                            );
                        }
                        if prepared
                            .pipelines
                            .fused
                            .iter()
                            .any(|f| index >= f.first && index < f.last)
                        {
                            #[cfg(test)]
                            if self.profile {
                                for offset in 1..=2 {
                                    d.cmd_write_timestamp(
                                        cmd,
                                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                                        flight.query,
                                        profile_query + offset,
                                    );
                                }
                            }
                            continue;
                        }
                        p.core[2] = header;
                        let halo = prepared.halos[layer.output] as u32;
                        p.active = [
                            pool.halo - halo,
                            pool.halo - halo,
                            p.core[0] + halo * 2,
                            p.core[1] + halo * 2,
                        ];
                        d.cmd_push_constants(
                            cmd,
                            prepared.pipelines.layout,
                            vk::ShaderStageFlags::COMPUTE,
                            0,
                            bytes(&[p]),
                        );
                        let fused = prepared.pipelines.fused.iter().find(|f| f.last == index);
                        d.cmd_bind_pipeline(
                            cmd,
                            vk::PipelineBindPoint::COMPUTE,
                            fused.map_or(prepared.pipelines.conv[index], |f| f.pipeline),
                        );
                        if let Some(f) = fused {
                            d.cmd_dispatch(
                                cmd,
                                p.active[2].div_ceil(f.tile[0]),
                                p.active[3].div_ceil(f.tile[1]),
                                1,
                            );
                        } else if layer.output == prepared.model.head.tensor
                            && prepared.pipelines.vector_head
                        {
                            d.cmd_dispatch(
                                cmd,
                                p.active[2].div_ceil(8),
                                p.active[3].div_ceil(8),
                                1,
                            );
                        } else {
                            d.cmd_dispatch(
                                cmd,
                                p.active[2].div_ceil(
                                    self.gpu.shape[1]
                                        * prepared.pipelines.layer_groups[index]
                                        * prepared.pipelines.strips[index],
                                ),
                                prepared.packed.metadata[header as usize + 15]
                                    / (self.gpu.shape[0] * prepared.pipelines.blocks[index]),
                                p.active[3],
                            );
                        }
                        #[cfg(test)]
                        if self.profile {
                            d.cmd_write_timestamp(
                                cmd,
                                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                                flight.query,
                                profile_query + 1,
                            );
                        }
                        if layer.border.is_some()
                            && (x <= pool.halo
                                || y <= pool.halo
                                || x + pool.cw + pool.halo >= pool.width
                                || y + pool.ch + pool.halo >= pool.height)
                        {
                            compute_barrier(d, cmd);
                            d.cmd_bind_pipeline(
                                cmd,
                                vk::PipelineBindPoint::COMPUTE,
                                prepared.pipelines.border[index],
                            );
                            d.cmd_dispatch(
                                cmd,
                                ((2 * pool.width + 2 * pool.height) * layer.out_channels as u32)
                                    .div_ceil(128),
                                1,
                                1,
                            );
                        }
                        compute_barrier(d, cmd);
                        if prepared.model.padding == Padding::Clamp
                            && layer.output != prepared.model.head.tensor
                        {
                            d.cmd_bind_pipeline(
                                cmd,
                                vk::PipelineBindPoint::COMPUTE,
                                prepared.pipelines.pad,
                            );
                            d.cmd_dispatch(cmd, (p.active[2] * p.active[3]).div_ceil(128), 1, 1);
                            compute_barrier(d, cmd);
                        }
                        #[cfg(test)]
                        if self.profile {
                            d.cmd_write_timestamp(
                                cmd,
                                vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                                flight.query,
                                profile_query + 2,
                            );
                        }
                    }
                }
            }
            let release: Vec<_> = [&pool.input, &pool.output]
                .iter()
                .map(|i| {
                    vk::ImageMemoryBarrier::default()
                        .image(i.image)
                        .old_layout(vk::ImageLayout::GENERAL)
                        .new_layout(vk::ImageLayout::GENERAL)
                        .src_queue_family_index(self.gpu.family)
                        .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
                        .src_access_mask(
                            vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE,
                        )
                        .subresource_range(image_range())
                })
                .collect();
            d.cmd_pipeline_barrier(
                cmd,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &release,
            );
            if self.gpu.timestamp_bits > 0 {
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::BOTTOM_OF_PIPE, flight.query, 1);
            }
            d.end_command_buffer(cmd).map_err(error)
        }
    }
}

unsafe fn compute_barrier(d: &ash::Device, cmd: vk::CommandBuffer) {
    unsafe {
        d.cmd_pipeline_barrier(
            cmd,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::PipelineStageFlags::COMPUTE_SHADER,
            vk::DependencyFlags::empty(),
            &[vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::SHADER_WRITE | vk::AccessFlags::SHADER_READ)
                .dst_access_mask(vk::AccessFlags::SHADER_READ | vk::AccessFlags::SHADER_WRITE)],
            &[],
            &[],
        );
    }
}

fn copy_shader(device: &ID3D11Device) -> Result<ID3D11ComputeShader> {
    let source = include_bytes!("vk_coopmat/copy.hlsl");
    unsafe {
        let mut code = None;
        let mut messages = None;
        if let Err(e) = D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR::null(),
            None,
            None,
            PCSTR(c"main".as_ptr().cast()),
            PCSTR(c"cs_5_0".as_ptr().cast()),
            0,
            0,
            &mut code,
            Some(&mut messages),
        ) {
            let message = messages.map(|b| {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer().cast(),
                    b.GetBufferSize(),
                ))
                .into_owned()
            });
            return Err(error(format!(
                "copy shader: {}",
                message.unwrap_or_else(|| e.to_string())
            )));
        }
        let code = code.ok_or_else(|| error("D3DCompile returned null"))?;
        let mut shader = None;
        device
            .CreateComputeShader(
                std::slice::from_raw_parts(code.GetBufferPointer().cast(), code.GetBufferSize()),
                None,
                Some(&mut shader),
            )
            .map_err(error)?;
        shader.ok_or_else(|| error("CreateComputeShader returned null"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::pipeline::LumaUpscalerMode;
    use crate::renderer::sr::{catalog, model::*, reference};
    use std::time::Instant;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_SRV_DIMENSION_TEXTURE2DARRAY,
    };

    #[test]
    fn vk_half_head_requires_the_validated_weights() {
        for mode in [
            LumaUpscalerMode::ArtCnnC4F16Dn,
            LumaUpscalerMode::ArtCnnC4F32Dn,
        ] {
            let model = catalog::execution_model(mode).unwrap();
            assert!(validated_half_head_model(&model));
            let mut custom = (*model).clone();
            custom.layers.last_mut().unwrap().inputs[0].weights[0] += 0.125;
            assert!(!validated_half_head_model(&custom));
        }
        let original = catalog::source_model(LumaUpscalerMode::ArtCnnC4F16Dn).unwrap();
        assert!(!validated_half_head_model(&original));
    }

    #[test]
    fn vk_half_head_uniform_layout() {
        let packed = pack_half_head_weights(&[1.0, -2.0, 0.5, 0.0, -0.0, 4.0, -8.0, 0.25]);
        assert_eq!(std::mem::size_of_val(packed.as_slice()), 32);
        assert_eq!(packed[0], [0xc0003c00, 0x00003800, 0, 0]);
        assert_eq!(packed[1], [0x44008000, 0x3400c800, 0, 0]);
    }

    fn hardware() -> (ID3D11Device, ID3D11DeviceContext) {
        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                Default::default(),
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
            .unwrap();
        }
        (device.unwrap(), context.unwrap())
    }

    #[test]
    fn vk_packing_strides() {
        for mode in [
            LumaUpscalerMode::ArtCnnC4F16,
            LumaUpscalerMode::ArtCnnC4F32,
            LumaUpscalerMode::ArtCnnR8F64,
            LumaUpscalerMode::Anime4kM,
            LumaUpscalerMode::Anime4kUl,
        ] {
            let model = catalog::execution_model(mode).unwrap();
            let packed = pack_model(&model, [16, 16, 16, 32]);
            for (&header, layer) in packed.headers.iter().zip(&model.layers) {
                let h = header as usize;
                let first = packed.metadata[h + 13] as usize;
                if layer
                    .inputs
                    .iter()
                    .all(|i| model.tensors[i.tensor] == model.tensors[layer.inputs[0].tensor])
                {
                    for t in 0..packed.metadata[h + 14] as usize {
                        assert_eq!(
                            packed.metadata[first + 8 * t + 5],
                            packed.metadata[first + 5]
                                + t as u32 * packed.metadata[h + 15] * packed.metadata[first + 4],
                            "{mode:?} layer {header} tap {t}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "driver shader diagnostics; set ERIKA_VK_STATS=1"]
    fn vk_shader_statistics() {
        let (device, _) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        assert!(
            up.gpu.capture,
            "requires ERIKA_VK_STATS=1 and pipeline executable properties"
        );
        up.prepare(catalog::execution_model(LumaUpscalerMode::ArtCnnC4F32).unwrap())
            .unwrap();
        unsafe {
            let api = ash::khr::pipeline_executable_properties::Device::new(
                &up.gpu.instance.0,
                &up.gpu.device,
            );
            for (i, &pipeline) in up
                .prepared
                .as_ref()
                .unwrap()
                .pipelines
                .conv
                .iter()
                .enumerate()
            {
                let properties = api
                    .get_pipeline_executable_properties(
                        &vk::PipelineInfoKHR::default().pipeline(pipeline),
                    )
                    .unwrap();
                for (j, p) in properties.iter().enumerate() {
                    eprintln!(
                        "SHADER layer={i} executable={j} subgroup={} {}",
                        p.subgroup_size,
                        CStr::from_ptr(p.description.as_ptr()).to_string_lossy()
                    );
                    let info = vk::PipelineExecutableInfoKHR::default()
                        .pipeline(pipeline)
                        .executable_index(j as u32);
                    for stat in api.get_pipeline_executable_statistics(&info).unwrap() {
                        let value = match stat.format {
                            vk::PipelineExecutableStatisticFormatKHR::UINT64 => {
                                stat.value.u64 as f64
                            }
                            vk::PipelineExecutableStatisticFormatKHR::INT64 => {
                                stat.value.i64 as f64
                            }
                            vk::PipelineExecutableStatisticFormatKHR::FLOAT64 => stat.value.f64,
                            _ => stat.value.b32 as f64,
                        };
                        eprintln!(
                            "STAT {i} {} {value}",
                            CStr::from_ptr(stat.name.as_ptr()).to_string_lossy()
                        );
                    }
                    let mut reps = api
                        .get_pipeline_executable_internal_representations(&info)
                        .unwrap();
                    let mut buffers: Vec<Vec<u8>> =
                        reps.iter().map(|r| vec![0; r.data_size]).collect();
                    for (rep, buffer) in reps.iter_mut().zip(&mut buffers) {
                        rep.p_data = buffer.as_mut_ptr().cast();
                    }
                    let mut count = reps.len() as u32;
                    let status = (api
                        .fp()
                        .get_pipeline_executable_internal_representations_khr)(
                        up.gpu.device.handle(),
                        &info,
                        &mut count,
                        reps.as_mut_ptr(),
                    );
                    if status == vk::Result::SUCCESS {
                        for (k, (rep, buffer)) in reps.iter().zip(&buffers).enumerate() {
                            eprintln!(
                                "REP {i} {j} {k} {} bytes={}",
                                CStr::from_ptr(rep.name.as_ptr()).to_string_lossy(),
                                rep.data_size
                            );
                            std::fs::write(format!("vk_shader_{i}_{j}_{k}.txt"), buffer).unwrap();
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "per-layer Vulkan timestamp diagnostics"]
    fn vk_layer_profile() {
        let (device, context) = hardware();
        for mode in [
            LumaUpscalerMode::ArtCnnC4F16,
            LumaUpscalerMode::ArtCnnC4F32,
            LumaUpscalerMode::Anime4kUl,
            LumaUpscalerMode::ArtCnnR8F64,
        ] {
            let (w, h) = if mode == LumaUpscalerMode::ArtCnnR8F64 {
                (1280, 720)
            } else {
                (1920, 1080)
            };
            let input = upload(
                &device,
                &reference::test_pattern(w, h, 7),
                w as u32,
                h as u32,
                DXGI_FORMAT_R32_FLOAT,
                0,
            );
            let mut up = VkCoopMatUpscaler::new(&device).unwrap();
            up.profile = true;
            let model = catalog::execution_model(mode).unwrap();
            for (i, l) in model.layers.iter().enumerate() {
                eprintln!(
                    "GRAPH {mode:?} layer={i} tensor={} out={} inputs={:?} residual={:?} activation={:?} border={}",
                    l.output,
                    l.out_channels,
                    l.inputs
                        .iter()
                        .map(|v| (v.tensor, model.tensors[v.tensor], v.kernel, v.transform))
                        .collect::<Vec<_>>(),
                    l.residual,
                    l.activation,
                    l.border.is_some()
                );
            }
            for half in [false, true] {
                if half && !up.gpu.half_supported {
                    continue;
                }
                up.half_accumulation = half;
                up.prepare(model.clone()).unwrap();
                for _ in 0..3 {
                    up.encode(
                        &device,
                        &context,
                        &input,
                        w as u32,
                        h as u32,
                        LumaRange::Full,
                    )
                    .unwrap();
                    up.idle().unwrap();
                }
            }
        }
    }

    fn upload(
        device: &ID3D11Device,
        values: &[f32],
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
        slice: u32,
    ) -> ID3D11ShaderResourceView {
        let quantized: Vec<u8> = match format {
            DXGI_FORMAT_R8_UNORM => values.iter().map(|v| (v * 255.0).round() as u8).collect(),
            DXGI_FORMAT_R16_UNORM => values
                .iter()
                .flat_map(|v| ((v * 65535.0).round() as u16).to_le_bytes())
                .collect(),
            _ => bytes(values).to_vec(),
        };
        let bpp = quantized.len() as u32 / (w * h);
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: slice + 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let initial = vec![
            D3D11_SUBRESOURCE_DATA {
                pSysMem: quantized.as_ptr().cast(),
                SysMemPitch: w * bpp,
                SysMemSlicePitch: 0
            };
            (slice + 1) as usize
        ];
        unsafe {
            let mut texture = None;
            device
                .CreateTexture2D(&desc, Some(initial.as_ptr()), Some(&mut texture))
                .unwrap();
            let view = D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                        MostDetailedMip: 0,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                    },
                },
            };
            let mut srv = None;
            device
                .CreateShaderResourceView(texture.as_ref().unwrap(), Some(&view), Some(&mut srv))
                .unwrap();
            srv.unwrap()
        }
    }

    fn readback(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        srv: &ID3D11ShaderResourceView,
    ) -> Vec<f32> {
        unsafe {
            let texture: ID3D11Texture2D = srv.GetResource().unwrap().cast().unwrap();
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.MiscFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut staging = None;
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .unwrap();
            let staging = staging.unwrap();
            context.CopyResource(&staging, &texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .unwrap();
            let mut values = Vec::new();
            for y in 0..desc.Height {
                for x in 0..desc.Width {
                    let ptr = mapped
                        .pData
                        .cast::<u8>()
                        .add((y * mapped.RowPitch + x * 2) as usize);
                    values.push(f16_to_f32(u16::from_le_bytes([*ptr, *ptr.add(1)])));
                }
            }
            context.Unmap(&staging, 0);
            values
        }
    }

    fn compare(actual: &[f32], expected: &[f32], label: &str, assert_accuracy: bool) {
        assert_eq!(actual.len(), expected.len());
        assert!(actual.iter().all(|x| x.is_finite()), "nonfinite {label}");
        let sum: f64 = actual
            .iter()
            .zip(expected)
            .map(|(a, b)| f64::from((a - b).abs()))
            .sum();
        let max = actual
            .iter()
            .zip(expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        let mae = sum / actual.len() as f64;
        eprintln!("ORACLE {label} mae={mae:.8} max={max:.8}");
        if assert_accuracy {
            assert!(mae < 2e-3 && max < 3e-2, "{label}: mae={mae} max={max}");
        }
    }

    fn check(mode: LumaUpscalerMode) {
        let (device, context) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).expect("hardware cooperative matrix backend");
        eprintln!("SHAPE {:?} half={}", up.gpu.shape, up.gpu.half_supported);
        let source = catalog::source_model(mode).unwrap();
        let optimized = catalog::execution_model(mode).unwrap();
        let sizes = if mode == LumaUpscalerMode::ArtCnnR8F64 {
            vec![(17, 13), (33, 19), (67, 45), (129, 77)]
        } else {
            vec![(1, 1), (2, 3), (67, 45), (129, 77)]
        };
        // Both accumulation variants must meet the same oracle thresholds.
        for half in [false, true] {
            if half && !up.gpu.half_supported {
                continue;
            }
            up.half_accumulation = half;
            up.prepare(optimized.clone()).unwrap();
            for &(w, h) in &sizes {
                for range in [LumaRange::Full, LumaRange::Limited] {
                    let values = reference::test_pattern(w, h, 17);
                    let luma = upload(
                        &device,
                        &values,
                        w as u32,
                        h as u32,
                        DXGI_FORMAT_R32_FLOAT,
                        0,
                    );
                    let output = up
                        .encode(&device, &context, &luma, w as u32, h as u32, range)
                        .unwrap();
                    let actual = readback(&device, &context, &output);
                    // Folded three-class border weights cannot represent both
                    // first and last simultaneously at extent one. Check the
                    // graph actually supplied to prepare for that degenerate
                    // case, and the published source for all ordinary sizes.
                    let oracle = if w == 1 || h == 1 {
                        &optimized
                    } else {
                        &source
                    };
                    let expected = reference::run(oracle, &values, w, h, range);
                    compare(
                        &actual,
                        &expected,
                        &format!(
                            "{mode:?} {w}x{h} {range:?} fp{}",
                            if half { 16 } else { 32 }
                        ),
                        true,
                    );
                }
            }
        }
    }

    #[test]
    fn vk_oracle_c4f16() {
        check(LumaUpscalerMode::ArtCnnC4F16);
    }
    #[test]
    fn vk_oracle_c4f32() {
        check(LumaUpscalerMode::ArtCnnC4F32);
    }
    #[test]
    fn vk_oracle_r8f64() {
        check(LumaUpscalerMode::ArtCnnR8F64);
    }
    #[test]
    fn vk_oracle_anime_m() {
        check(LumaUpscalerMode::Anime4kM);
    }
    #[test]
    fn vk_oracle_anime_ul() {
        check(LumaUpscalerMode::Anime4kUl);
    }

    #[test]
    fn vk_remaining_catalog_modes() {
        let (device, context) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        let (w, h) = (67, 45);
        let values = reference::test_pattern(w, h, 31);
        let input = upload(
            &device,
            &values,
            w as u32,
            h as u32,
            DXGI_FORMAT_R32_FLOAT,
            0,
        );
        for mode in LumaUpscalerMode::ALL {
            if matches!(
                mode,
                LumaUpscalerMode::ArtCnnC4F16
                    | LumaUpscalerMode::ArtCnnC4F32
                    | LumaUpscalerMode::ArtCnnR8F64
                    | LumaUpscalerMode::Anime4kM
                    | LumaUpscalerMode::Anime4kUl
            ) {
                continue;
            }
            let Ok(model) = catalog::execution_model(mode) else {
                continue;
            };
            let source = catalog::source_model(mode).unwrap();
            for half in [false, true] {
                if half && !up.gpu.half_supported {
                    continue;
                }
                up.half_accumulation = half;
                up.prepare(model.clone()).unwrap();
                for range in [LumaRange::Full, LumaRange::Limited] {
                    let output = up
                        .encode(&device, &context, &input, w as u32, h as u32, range)
                        .unwrap();
                    compare(
                        &readback(&device, &context, &output),
                        &reference::run(&source, &values, w, h, range),
                        &format!(
                            "{mode:?} {w}x{h} {range:?} fp{}",
                            if half { 16 } else { 32 }
                        ),
                        true,
                    );
                }
            }
        }
    }

    #[test]
    fn vk_custom_graph_and_unorm_views() {
        let (device, context) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        up.tile = [64, 32];
        // An intentionally non-catalog DAG covers large kernels, multi-input
        // transforms, residuals, SiLU/PReLU and column-major bilinear output.
        let first = ConvLayer {
            output: 1,
            out_channels: 4,
            inputs: vec![ConvInput {
                tensor: 0,
                transform: InputTransform::Identity,
                kernel: 9,
                weights: (0..4 * 81).map(|i| (i as f32 * 0.1).sin() * 0.01).collect(),
            }],
            bias: vec![0.02; 4],
            residual: None,
            activation: Activation::Silu,
            border: None,
        };
        let mut second = ConvLayer {
            output: 2,
            out_channels: 4,
            inputs: vec![
                ConvInput {
                    tensor: 1,
                    transform: InputTransform::Relu,
                    kernel: 3,
                    weights: vec![0.01; 4 * 4 * 9],
                },
                ConvInput {
                    tensor: 1,
                    transform: InputTransform::NegRelu,
                    kernel: 1,
                    weights: vec![-0.03; 16],
                },
            ],
            bias: vec![-0.02; 4],
            residual: Some(1),
            activation: Activation::Prelu(vec![0.2, 0.3, 0.4, 0.5]),
            border: None,
        };
        second.border = Some(
            (0..9)
                .map(|i| LayerWeights {
                    inputs: second
                        .inputs
                        .iter()
                        .map(|input| {
                            input
                                .weights
                                .iter()
                                .map(|v| v * (0.5 + i as f32 * 0.1))
                                .collect()
                        })
                        .collect(),
                    bias: vec![i as f32 * 0.01; 4],
                })
                .collect(),
        );
        second.border.as_mut().unwrap()[4] = LayerWeights {
            inputs: second.inputs.iter().map(|i| i.weights.clone()).collect(),
            bias: second.bias.clone(),
        };
        for padding in [Padding::Zero, Padding::Clamp] {
            let model = Arc::new(SrModel {
                name: "Vulkan semantics".into(),
                padding,
                input_range: InputRange::Full,
                tensors: vec![1, 4, 4],
                layers: vec![first.clone(), second.clone()],
                head: Head {
                    tensor: 2,
                    order: SubpixelOrder::ColumnMajor,
                    base: HeadBase::Bilinear,
                    clamp: false,
                },
            });
            up.prepare(model.clone()).unwrap();
            for (w, h) in [(1, 1), (2, 3), (67, 45), (521, 137)] {
                for format in [
                    DXGI_FORMAT_R8_UNORM,
                    DXGI_FORMAT_R16_UNORM,
                    DXGI_FORMAT_R32_FLOAT,
                ] {
                    let mut values = reference::test_pattern(w, h, 19);
                    if format == DXGI_FORMAT_R8_UNORM {
                        for x in &mut values {
                            *x = (*x * 255.0).round() / 255.0;
                        }
                    }
                    if format == DXGI_FORMAT_R16_UNORM {
                        for x in &mut values {
                            *x = (*x * 65535.0).round() / 65535.0;
                        }
                    }
                    let luma = upload(&device, &values, w as u32, h as u32, format, 1);
                    let output = up
                        .encode(
                            &device,
                            &context,
                            &luma,
                            w as u32,
                            h as u32,
                            LumaRange::Limited,
                        )
                        .unwrap();
                    compare(
                        &readback(&device, &context, &output),
                        &reference::run(&model, &values, w, h, LumaRange::Limited),
                        &format!("custom {w}x{h} {padding:?} {format:?}"),
                        true,
                    );
                }
            }
        }
    }

    #[test]
    fn vk_1080p_smoke_and_reuse() {
        let (device, context) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        up.tile = [512, 256];
        up.prepare(catalog::execution_model(LumaUpscalerMode::ArtCnnC4F16).unwrap())
            .unwrap();
        let values = reference::test_pattern(1920, 1080, 3);
        let input = upload(&device, &values, 1920, 1080, DXGI_FORMAT_R32_FLOAT, 0);
        for iteration in 0..3 {
            let output = up
                .encode(&device, &context, &input, 1920, 1080, LumaRange::Limited)
                .unwrap();
            let actual = readback(&device, &context, &output);
            assert_eq!(actual.len(), 1920 * 1080 * 4);
            assert!(
                actual
                    .iter()
                    .all(|&v| v.is_finite() && (0.0..=1.0).contains(&v))
            );
            if iteration == 2 {
                let source = catalog::source_model(LumaUpscalerMode::ArtCnnC4F16).unwrap();
                // Reference patches cover true image edges and a core-tile
                // intersection. Exclude only the artificial crop's halo.
                for (ox, oy) in [(0, 0), (991, 234), (1853, 1035)] {
                    let mut crop = Vec::new();
                    for y in 0..45 {
                        crop.extend_from_slice(
                            &values[(oy + y) * 1920 + ox..(oy + y) * 1920 + ox + 67],
                        );
                    }
                    let expected = reference::run(&source, &crop, 67, 45, LumaRange::Limited);
                    let mut a = Vec::new();
                    let mut b = Vec::new();
                    let left = if ox == 0 { 0 } else { 7 };
                    let top = if oy == 0 { 0 } else { 7 };
                    let right = if ox + 67 == 1920 { 67 } else { 60 };
                    let bottom = if oy + 45 == 1080 { 45 } else { 38 };
                    for y in top..bottom {
                        for x in left..right {
                            for sy in 0..2 {
                                for sx in 0..2 {
                                    a.push(actual[(2 * (oy + y) + sy) * 3840 + 2 * (ox + x) + sx]);
                                    b.push(expected[(2 * y + sy) * 134 + 2 * x + sx]);
                                }
                            }
                        }
                    }
                    compare(&a, &b, &format!("1080p source patch {ox},{oy}"), true);
                }
            }
        }
        up.collect().unwrap();
        assert!(up.last_gpu_duration().unwrap() > Duration::ZERO);
        assert!(up.pool.as_ref().unwrap()._arena.size <= ARENA_LIMIT);
    }

    fn query(device: &ID3D11Device, kind: D3D11_QUERY) -> ID3D11Query {
        let mut q = None;
        unsafe {
            device
                .CreateQuery(
                    &D3D11_QUERY_DESC {
                        Query: kind,
                        MiscFlags: 0,
                    },
                    Some(&mut q),
                )
                .unwrap();
        }
        q.unwrap()
    }

    fn poll<T: Default>(
        context: &ID3D11DeviceContext,
        query: &ID3D11Query,
        ready: impl Fn(&T) -> bool,
    ) -> T {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let mut value = T::default();
            unsafe {
                context
                    .GetData(
                        query,
                        Some((&mut value as *mut T).cast()),
                        mem::size_of::<T>() as u32,
                        0,
                    )
                    .unwrap();
            }
            // windows-rs maps both S_OK and S_FALSE to Ok. Inspect the
            // sentinel instead of treating Ok as proof of availability.
            if ready(&value) {
                return value;
            }
            assert!(Instant::now() < deadline, "GPU query timeout");
            std::thread::yield_now();
        }
    }

    #[test]
    #[ignore = "hardware GPU benchmark; run release, single test thread"]
    fn vk_gpu_benchmark() {
        let (device, context) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        eprintln!("SHAPE {:?}", up.gpu.shape);
        for mode in [
            LumaUpscalerMode::ArtCnnC4F16,
            LumaUpscalerMode::ArtCnnC4F32,
            LumaUpscalerMode::ArtCnnR8F64,
            LumaUpscalerMode::Anime4kM,
            LumaUpscalerMode::Anime4kUl,
        ] {
            let quick = std::env::var_os("ERIKA_VK_BENCH_QUICK").is_some();
            if quick && [LumaUpscalerMode::ArtCnnR8F64, LumaUpscalerMode::Anime4kM].contains(&mode)
            {
                continue;
            }
            let model = catalog::execution_model(mode).unwrap();
            for half in [false, true] {
                if half && !up.gpu.half_supported {
                    continue;
                }
                up.half_accumulation = half;
                if let Err(e) = up.prepare(model.clone()) {
                    if quick {
                        eprintln!("UNSUPPORTED {mode:?} fp{} {e}", if half { 16 } else { 32 });
                        continue;
                    }
                    panic!("prepare: {e}");
                }
                for (w, h) in [(1280, 720), (1920, 1080), (3840, 2160)] {
                    if quick && w != 1920 {
                        continue;
                    }
                    if mode == LumaUpscalerMode::ArtCnnR8F64 && w > 1920 {
                        continue;
                    }
                    let input = upload(
                        &device,
                        &reference::test_pattern(w, h, 7),
                        w as u32,
                        h as u32,
                        DXGI_FORMAT_R32_FLOAT,
                        0,
                    );
                    let start = query(&device, D3D11_QUERY_TIMESTAMP);
                    let end = query(&device, D3D11_QUERY_TIMESTAMP);
                    let disjoint = query(&device, D3D11_QUERY_TIMESTAMP_DISJOINT);
                    for _ in 0..2 {
                        up.encode(
                            &device,
                            &context,
                            &input,
                            w as u32,
                            h as u32,
                            LumaRange::Full,
                        )
                        .unwrap();
                    }
                    let mut total = Vec::new();
                    let mut kernel = Vec::new();
                    for _ in 0..5 {
                        unsafe {
                            context.Begin(&disjoint);
                            context.End(&start);
                        }
                        up.encode(
                            &device,
                            &context,
                            &input,
                            w as u32,
                            h as u32,
                            LumaRange::Full,
                        )
                        .unwrap();
                        unsafe {
                            context.End(&end);
                            context.End(&disjoint);
                            context.Flush();
                        }
                        let b: u64 = poll(&context, &end, |v| *v != 0);
                        let a: u64 = poll(&context, &start, |v| *v != 0);
                        let freq: D3D11_QUERY_DATA_TIMESTAMP_DISJOINT = poll(
                            &context,
                            &disjoint,
                            |v: &D3D11_QUERY_DATA_TIMESTAMP_DISJOINT| v.Frequency != 0,
                        );
                        assert!(!freq.Disjoint.as_bool());
                        total.push((b - a) as f64 / freq.Frequency as f64 * 1000.0);
                        up.collect().unwrap();
                        kernel.push(up.last_gpu_duration().unwrap().as_secs_f64() * 1000.0);
                    }
                    total.sort_by(f64::total_cmp);
                    kernel.sort_by(f64::total_cmp);
                    let ops = 2.0 * model.macs_per_pixel() as f64 * (w * h) as f64;
                    eprintln!(
                        "BENCH {mode:?} {w}x{h} fp{} e2e_ms={:.4} kernel_ms={:.4} e2e_TFLOPS={:.4} kernel_TFLOPS={:.4} arena_MiB={:.2}",
                        if half { 16 } else { 32 },
                        total[2],
                        kernel[2],
                        ops / total[2] / 1e9,
                        ops / kernel[2] / 1e9,
                        up.pool.as_ref().unwrap()._arena.size as f64 / 1048576.0
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "hardware tile tuning; run release, single test thread"]
    fn vk_tile_sweep() {
        let (device, context) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        for mode in [
            LumaUpscalerMode::ArtCnnC4F16,
            LumaUpscalerMode::ArtCnnC4F32,
            LumaUpscalerMode::ArtCnnR8F64,
        ] {
            let model = catalog::execution_model(mode).unwrap();
            let (w, h) = if mode == LumaUpscalerMode::ArtCnnR8F64 {
                (320, 180)
            } else {
                (1280, 720)
            };
            let input = upload(
                &device,
                &reference::test_pattern(w, h, 7),
                w as u32,
                h as u32,
                DXGI_FORMAT_R32_FLOAT,
                0,
            );
            for half in [false, true] {
                for groups in [1, 2, 4, 8] {
                    for tile in [[256, 64], [512, 128], [1024, 256]] {
                        up.groups = groups;
                        up.tile = tile;
                        up.half_accumulation = half;
                        if up.prepare(model.clone()).is_err() {
                            continue;
                        }
                        for _ in 0..2 {
                            up.encode(
                                &device,
                                &context,
                                &input,
                                w as u32,
                                h as u32,
                                LumaRange::Full,
                            )
                            .unwrap();
                        }
                        up.idle().unwrap();
                        let mut times = Vec::new();
                        for _ in 0..3 {
                            up.encode(
                                &device,
                                &context,
                                &input,
                                w as u32,
                                h as u32,
                                LumaRange::Full,
                            )
                            .unwrap();
                            up.idle().unwrap();
                            times.push(up.last_gpu_duration().unwrap().as_secs_f64() * 1000.0);
                        }
                        times.sort_by(f64::total_cmp);
                        eprintln!(
                            "TILE {mode:?} {w}x{h} fp{} groups={groups} tile={}x{} ms={:.4} TFLOPS={:.4}",
                            if half { 16 } else { 32 },
                            tile[0],
                            tile[1],
                            times[1],
                            2.0 * model.macs_per_pixel() as f64 * (w * h) as f64 / times[1] / 1e9
                        );
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "resident matrix register/strip tuning; run release"]
    fn vk_resident_sweep() {
        let (device, context) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        let (w, h) = (1280, 720);
        let input = upload(
            &device,
            &reference::test_pattern(w, h, 7),
            w as u32,
            h as u32,
            DXGI_FORMAT_R32_FLOAT,
            0,
        );
        for mode in [
            LumaUpscalerMode::ArtCnnC4F16,
            LumaUpscalerMode::ArtCnnC4F32,
            LumaUpscalerMode::ArtCnnR8F64,
        ] {
            let model = catalog::execution_model(mode).unwrap();
            for half in [false, true] {
                for groups in [1, 2, 4] {
                    for strips in [1, 2, 4, 8] {
                        if half && !up.gpu.half_supported {
                            continue;
                        }
                        up.half_accumulation = half;
                        up.groups = groups;
                        up.strips = strips;
                        up.tile = [0, 0];
                        if up.prepare(model.clone()).is_err() {
                            continue;
                        }
                        up.encode(
                            &device,
                            &context,
                            &input,
                            w as u32,
                            h as u32,
                            LumaRange::Full,
                        )
                        .unwrap();
                        up.idle().unwrap();
                        let mut times = Vec::new();
                        for _ in 0..3 {
                            up.encode(
                                &device,
                                &context,
                                &input,
                                w as u32,
                                h as u32,
                                LumaRange::Full,
                            )
                            .unwrap();
                            up.idle().unwrap();
                            times.push(up.last_gpu_duration().unwrap().as_secs_f64() * 1000.0);
                        }
                        times.sort_by(f64::total_cmp);
                        let pool = up.pool.as_ref().unwrap();
                        eprintln!(
                            "RESIDENT {mode:?} {w}x{h} fp{} groups={groups} strips={strips} actual={} tile={}x{} ms={:.4} TFLOPS={:.4}",
                            if half { 16 } else { 32 },
                            up.prepared
                                .as_ref()
                                .unwrap()
                                .pipelines
                                .strips
                                .iter()
                                .max()
                                .unwrap(),
                            pool.cw,
                            pool.ch,
                            times[1],
                            2.0 * model.macs_per_pixel() as f64 * (w * h) as f64 / times[1] / 1e9
                        );
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "direct element epilogue strip tuning"]
    fn vk_native_sweep() {
        let (device, context) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        let (w, h) = (1920, 1080);
        let input = upload(
            &device,
            &reference::test_pattern(w, h, 7),
            w as u32,
            h as u32,
            DXGI_FORMAT_R32_FLOAT,
            0,
        );
        for mode in [
            LumaUpscalerMode::ArtCnnC4F16,
            LumaUpscalerMode::ArtCnnC4F32,
            LumaUpscalerMode::Anime4kUl,
        ] {
            let model = catalog::execution_model(mode).unwrap();
            for half in [false, true] {
                if half && !up.gpu.half_supported {
                    continue;
                }
                up.half_accumulation = half;
                for groups in [1, 2, 4] {
                    for strips in [1, 2, 4, 8] {
                        up.groups = groups;
                        up.strips = strips;
                        if up.prepare(model.clone()).is_err() {
                            continue;
                        }
                        let mut times = Vec::new();
                        for sample in 0..4 {
                            up.encode(
                                &device,
                                &context,
                                &input,
                                w as u32,
                                h as u32,
                                LumaRange::Full,
                            )
                            .unwrap();
                            up.idle().unwrap();
                            if sample > 0 {
                                times.push(up.last_gpu_duration().unwrap().as_secs_f64() * 1000.0);
                            }
                        }
                        times.sort_by(f64::total_cmp);
                        eprintln!(
                            "NATIVE {mode:?} fp{} groups={groups} strips={strips} actual={} ms={:.4}",
                            if half { 16 } else { 32 },
                            up.prepared
                                .as_ref()
                                .unwrap()
                                .pipelines
                                .strips
                                .iter()
                                .max()
                                .unwrap(),
                            times[1]
                        );
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "feature arena cache/dispatch tuning; run release"]
    fn vk_cache_sweep() {
        let (device, context) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        up.groups = 1;
        up.strips = 2;
        let (w, h) = (1280, 720);
        let input = upload(
            &device,
            &reference::test_pattern(w, h, 7),
            w as u32,
            h as u32,
            DXGI_FORMAT_R32_FLOAT,
            0,
        );
        for mode in [
            LumaUpscalerMode::ArtCnnC4F16,
            LumaUpscalerMode::ArtCnnC4F32,
            LumaUpscalerMode::ArtCnnR8F64,
        ] {
            let model = catalog::execution_model(mode).unwrap();
            for half in [false, true] {
                if half && !up.gpu.half_supported {
                    continue;
                }
                up.half_accumulation = half;
                for tile in [
                    [0, 0],
                    [256, 128],
                    [512, 256],
                    [1024, 256],
                    [1280, 360],
                    [1280, 720],
                ] {
                    up.tile = tile;
                    up.prepare(model.clone()).unwrap();
                    let mut times = Vec::new();
                    for sample in 0..4 {
                        up.encode(
                            &device,
                            &context,
                            &input,
                            w as u32,
                            h as u32,
                            LumaRange::Full,
                        )
                        .unwrap();
                        up.idle().unwrap();
                        if sample > 0 {
                            times.push(up.last_gpu_duration().unwrap().as_secs_f64() * 1000.0);
                        }
                    }
                    times.sort_by(f64::total_cmp);
                    let pool = up.pool.as_ref().unwrap();
                    eprintln!(
                        "CACHE {mode:?} {w}x{h} fp{} requested={}x{} actual={}x{} MiB={:.2} ms={:.4} TFLOPS={:.4}",
                        if half { 16 } else { 32 },
                        tile[0],
                        tile[1],
                        pool.cw,
                        pool.ch,
                        pool._arena.size as f64 / 1048576.0,
                        times[1],
                        2.0 * model.macs_per_pixel() as f64 * (w * h) as f64 / times[1] / 1e9
                    );
                }
            }
        }
    }

    #[test]
    #[ignore = "streaming device-memory bandwidth diagnostic"]
    fn vk_memory_bandwidth() {
        let (device, _) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        up.prepare(catalog::execution_model(LumaUpscalerMode::ArtCnnC4F32).unwrap())
            .unwrap();
        let prepared = up.prepared.as_ref().unwrap();
        let pool = Pool::new(&up.gpu, &device, prepared, 1920, 1080).unwrap();
        let count = 1u32 << 25;
        assert!(pool._arena.size >= u64::from(count) * 4);
        let pipeline = prepared
            .pipelines
            .compile(include_bytes!("vk_coopmat/bandwidth.spv"), &[])
            .unwrap();
        unsafe {
            let d = &up.gpu.device;
            let cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(up.commands)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .unwrap()[0];
            let query = d
                .create_query_pool(
                    &vk::QueryPoolCreateInfo::default()
                        .query_type(vk::QueryType::TIMESTAMP)
                        .query_count(2),
                    None,
                )
                .unwrap();
            let mut times = Vec::new();
            for sample in 0..7 {
                d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                    .unwrap();
                d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                    .unwrap();
                d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline);
                d.cmd_bind_descriptor_sets(
                    cmd,
                    vk::PipelineBindPoint::COMPUTE,
                    prepared.pipelines.layout,
                    0,
                    &[pool.set],
                    &[],
                );
                let mut p = Params {
                    size: [1920, 1080, pool.tw, pool.th],
                    origin: [0; 4],
                    core: [count, 0, u32::from(sample > 0), 0],
                    norm: [0.0; 4],
                    active: [0; 4],
                };
                d.cmd_push_constants(
                    cmd,
                    prepared.pipelines.layout,
                    vk::ShaderStageFlags::COMPUTE,
                    0,
                    bytes(&[p]),
                );
                if sample == 0 {
                    d.cmd_dispatch(cmd, 32768, 8, 1);
                    compute_barrier(d, cmd);
                    p.core[2] = 1;
                    d.cmd_push_constants(
                        cmd,
                        prepared.pipelines.layout,
                        vk::ShaderStageFlags::COMPUTE,
                        0,
                        bytes(&[p]),
                    );
                }
                d.cmd_reset_query_pool(cmd, query, 0, 2);
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, query, 0);
                d.cmd_dispatch(cmd, 32768, 4, 1);
                d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::BOTTOM_OF_PIPE, query, 1);
                d.end_command_buffer(cmd).unwrap();
                d.queue_submit(
                    up.gpu.queue,
                    &[vk::SubmitInfo::default().command_buffers(&[cmd])],
                    vk::Fence::null(),
                )
                .unwrap();
                d.queue_wait_idle(up.gpu.queue).unwrap();
                let mut stamps = [0u64; 2];
                d.get_query_pool_results(query, 0, &mut stamps, vk::QueryResultFlags::TYPE_64)
                    .unwrap();
                if sample > 1 {
                    times.push(
                        (stamps[1] - stamps[0]) as f64
                            * f64::from(up.gpu.limits.timestamp_period)
                            * 1e-6,
                    );
                }
            }
            times.sort_by(f64::total_cmp);
            eprintln!(
                "BANDWIDTH working_MiB=128 transferred_MiB=128 ms={:.5} GBs={:.3}",
                times[2],
                f64::from(count) * 4.0 / times[2] / 1e6
            );
            d.destroy_query_pool(query, None);
            d.free_command_buffers(up.commands, &[cmd]);
            d.destroy_pipeline(pipeline, None);
        }
    }

    #[test]
    #[ignore = "register-only cooperative matrix throughput ceiling"]
    fn vk_tensor_peak() {
        let (device, _) = hardware();
        let mut up = VkCoopMatUpscaler::new(&device).unwrap();
        up.prepare(catalog::execution_model(LumaUpscalerMode::ArtCnnC4F32).unwrap())
            .unwrap();
        let prepared = up.prepared.as_ref().unwrap();
        let pool = Pool::new(&up.gpu, &device, prepared, 512, 512).unwrap();
        unsafe {
            let d = &up.gpu.device;
            let cmd = d
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(up.commands)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .unwrap()[0];
            let query = d
                .create_query_pool(
                    &vk::QueryPoolCreateInfo::default()
                        .query_type(vk::QueryType::TIMESTAMP)
                        .query_count(2),
                    None,
                )
                .unwrap();
            for half in [false, true] {
                if half && !up.gpu.half_supported {
                    continue;
                }
                let pipeline = prepared
                    .pipelines
                    .compile(
                        if half {
                            include_bytes!("vk_coopmat/peak_half.spv")
                        } else {
                            include_bytes!("vk_coopmat/peak.spv")
                        },
                        &up.gpu.shape,
                    )
                    .unwrap();
                let p = Params {
                    size: [129, 77, pool.tw, pool.th],
                    origin: [0; 4],
                    core: [0, 0, 4096, 0],
                    norm: [0.0; 4],
                    active: [0; 4],
                };
                let mut times = Vec::new();
                for sample in 0..4 {
                    d.reset_command_buffer(cmd, vk::CommandBufferResetFlags::empty())
                        .unwrap();
                    d.begin_command_buffer(cmd, &vk::CommandBufferBeginInfo::default())
                        .unwrap();
                    d.cmd_reset_query_pool(cmd, query, 0, 2);
                    d.cmd_bind_pipeline(cmd, vk::PipelineBindPoint::COMPUTE, pipeline);
                    d.cmd_bind_descriptor_sets(
                        cmd,
                        vk::PipelineBindPoint::COMPUTE,
                        prepared.pipelines.layout,
                        0,
                        &[pool.set],
                        &[],
                    );
                    d.cmd_push_constants(
                        cmd,
                        prepared.pipelines.layout,
                        vk::ShaderStageFlags::COMPUTE,
                        0,
                        bytes(&[p]),
                    );
                    d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::TOP_OF_PIPE, query, 0);
                    d.cmd_dispatch(cmd, 256, 1, 1);
                    d.cmd_write_timestamp(cmd, vk::PipelineStageFlags::BOTTOM_OF_PIPE, query, 1);
                    d.end_command_buffer(cmd).unwrap();
                    d.queue_submit(
                        up.gpu.queue,
                        &[vk::SubmitInfo::default().command_buffers(&[cmd])],
                        vk::Fence::null(),
                    )
                    .unwrap();
                    d.queue_wait_idle(up.gpu.queue).unwrap();
                    let mut stamps = [0u64; 2];
                    d.get_query_pool_results(query, 0, &mut stamps, vk::QueryResultFlags::TYPE_64)
                        .unwrap();
                    if sample > 0 {
                        times.push(
                            (stamps[1] - stamps[0]) as f64
                                * f64::from(up.gpu.limits.timestamp_period)
                                * 1e-6,
                        );
                    }
                }
                times.sort_by(f64::total_cmp);
                let [m, n, k, _] = up.gpu.shape;
                let ops = 256.0 * 4.0 * 4096.0 * 4.0 * 2.0 * f64::from(m * n * k);
                eprintln!(
                    "PEAK fp{} ms={:.5} TFLOPS={:.3}",
                    if half { 16 } else { 32 },
                    times[1],
                    ops / times[1] / 1e9
                );
                d.destroy_pipeline(pipeline, None);
            }
            d.destroy_query_pool(query, None);
            d.free_command_buffers(up.commands, &[cmd]);
        }
    }
}
