//! Render targets backed by dedicated, exportable Vulkan memory. No pixels
//! pass through host memory. Published images are never written again.
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use ash::{khr, vk};

use crate::core::{ExternalTextureFormat, ExternalTextureInfo, ExternalTextureKind};

type Api = wgpu::hal::api::Vulkan;
const HANDLE_TYPE: vk::ExternalMemoryHandleTypeFlags = vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD;
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
const USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::COLOR_ATTACHMENT.as_raw()
        | vk::ImageUsageFlags::SAMPLED.as_raw()
        | vk::ImageUsageFlags::TRANSFER_SRC.as_raw(),
);

struct Allocation {
    raw: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
    fd: Option<OwnedFd>,
    // Owned only by the HAL texture's drop guard, which runs while its device
    // is alive. Holding a wgpu::Device here would create a retirement cycle.
}

impl Drop for Allocation {
    fn drop(&mut self) {
        unsafe {
            if self.image != vk::Image::null() {
                self.raw.destroy_image(self.image, None);
            }
            if self.memory != vk::DeviceMemory::null() {
                self.raw.free_memory(self.memory, None);
            }
        }
    }
}

pub(crate) struct VulkanExportTarget {
    pub texture: wgpu::Texture,
    pub info: ExternalTextureInfo,
    device: wgpu::Device,
    image: vk::Image,
    family: u32,
}

fn error(stage: &str, error: vk::Result) -> String {
    format!("Vulkan external texture {stage}: {error:?}")
}

impl VulkanExportTarget {
    pub fn new(device: &wgpu::Device, width: u32, height: u32) -> Result<Self, String> {
        let hal =
            unsafe { device.as_hal::<Api>() }.ok_or("native texture export requires Vulkan")?;
        if !hal
            .enabled_device_extensions()
            .contains(&khr::external_memory_fd::NAME)
        {
            return Err("VK_KHR_external_memory_fd is unavailable on this device".into());
        }
        let instance = hal.shared_instance().raw_instance();
        let physical = hal.raw_physical_device();
        let mut external =
            vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(HANDLE_TYPE);
        let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
            .format(FORMAT)
            .ty(vk::ImageType::TYPE_2D)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(USAGE)
            .push_next(&mut external);
        let mut external_properties = vk::ExternalImageFormatProperties::default();
        let mut properties =
            vk::ImageFormatProperties2::default().push_next(&mut external_properties);
        unsafe {
            instance.get_physical_device_image_format_properties2(
                physical,
                &format_info,
                &mut properties,
            )
        }
        .map_err(|e| error("format query", e))?;
        let limits = properties.image_format_properties;
        if width > limits.max_extent.width || height > limits.max_extent.height {
            return Err("external texture exceeds the Vulkan format extent limit".into());
        }
        if !external_properties
            .external_memory_properties
            .external_memory_features
            .contains(
                vk::ExternalMemoryFeatureFlags::EXPORTABLE
                    | vk::ExternalMemoryFeatureFlags::IMPORTABLE,
            )
        {
            return Err(
                "RGBA8 OPAQUE_FD render targets cannot be exported/imported on this GPU".into(),
            );
        }
        let mut identity = vk::PhysicalDeviceIDProperties::default();
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut identity);
        unsafe { instance.get_physical_device_properties2(physical, &mut properties) };
        let memory_properties = unsafe { instance.get_physical_device_memory_properties(physical) };
        let raw = hal.raw_device().clone();
        let family = hal.queue_family_index();
        let fd_api = khr::external_memory_fd::Device::new(instance, &raw);
        let mut external_image =
            vk::ExternalMemoryImageCreateInfo::default().handle_types(HANDLE_TYPE);
        let image_info = vk::ImageCreateInfo::default()
            .image_type(vk::ImageType::TYPE_2D)
            .format(FORMAT)
            .extent(vk::Extent3D {
                width,
                height,
                depth: 1,
            })
            .mip_levels(1)
            .array_layers(1)
            .samples(vk::SampleCountFlags::TYPE_1)
            .tiling(vk::ImageTiling::OPTIMAL)
            .usage(USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_image);
        let image = unsafe { raw.create_image(&image_info, None) }
            .map_err(|e| error("image allocation", e))?;
        let mut allocation = Allocation {
            raw,
            image,
            memory: vk::DeviceMemory::null(),
            fd: None,
        };
        let requirements = unsafe { allocation.raw.get_image_memory_requirements(image) };
        let memory_type_index = (0..memory_properties.memory_type_count)
            .filter(|index| requirements.memory_type_bits & (1 << index) != 0)
            .max_by_key(|&index| {
                memory_properties.memory_types[index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::DEVICE_LOCAL)
            })
            .ok_or("external image has no compatible memory type")?;
        let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(HANDLE_TYPE);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let memory_info = vk::MemoryAllocateInfo::default()
            .allocation_size(requirements.size)
            .memory_type_index(memory_type_index)
            .push_next(&mut export)
            .push_next(&mut dedicated);
        allocation.memory = unsafe { allocation.raw.allocate_memory(&memory_info, None) }
            .map_err(|e| error("memory allocation", e))?;
        unsafe {
            allocation
                .raw
                .bind_image_memory(image, allocation.memory, 0)
        }
        .map_err(|e| error("bind memory", e))?;
        let fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(allocation.memory)
            .handle_type(HANDLE_TYPE);
        let fd = unsafe { fd_api.get_memory_fd(&fd_info) }.map_err(|e| error("export fd", e))?;
        allocation.fd = Some(unsafe { OwnedFd::from_raw_fd(fd) });
        let info = ExternalTextureInfo {
            kind: ExternalTextureKind::VulkanOpaqueFd,
            format: ExternalTextureFormat::Rgba8Unorm,
            width,
            height,
            native_handle: allocation
                .fd
                .as_ref()
                .ok_or("missing export fd")?
                .as_raw_fd() as u64,
            device_id: 0,
            allocation_size: requirements.size,
            memory_type_index,
            vulkan_format: FORMAT.as_raw() as u32,
            vulkan_usage: USAGE.as_raw(),
            vulkan_tiling: vk::ImageTiling::OPTIMAL.as_raw() as u32,
            device_uuid: identity.device_uuid,
            driver_uuid: identity.driver_uuid,
        };
        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let descriptor = wgpu::TextureDescriptor {
            label: Some("erika-external-frame"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        };
        let hal_descriptor = wgpu::hal::TextureDescriptor {
            label: descriptor.label,
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: descriptor.dimension,
            format: descriptor.format,
            usage: wgpu::TextureUses::COLOR_TARGET
                | wgpu::TextureUses::RESOURCE
                | wgpu::TextureUses::COPY_SRC,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        let texture = unsafe {
            hal.texture_from_raw(
                image,
                &hal_descriptor,
                Some(Box::new(move || drop(allocation))),
                wgpu::hal::vulkan::TextureMemory::External,
            )
        };
        drop(hal);
        let texture = unsafe { device.create_texture_from_hal::<Api>(texture, &descriptor) };
        Ok(Self {
            texture,
            info,
            device: device.clone(),
            image,
            family,
        })
    }

    /// This image is never used by wgpu again after the ownership release.
    pub fn release_to_consumer(&self, queue: &wgpu::Queue) -> Result<(), String> {
        // Register the texture with this submission before using a raw barrier.
        // In particular, a completion timeout must not retire its allocation
        // after the earlier render submission while this release is in flight.
        // wgpu 29 forbids mixing its regular and raw encoding APIs on one
        // encoder, so submit these two command buffers together, in order.
        let mut tracked = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("erika-external-lifetime"),
            });
        tracked.transition_resources(
            std::iter::empty(),
            std::iter::once(wgpu::TextureTransition {
                texture: &self.texture,
                selector: None,
                state: wgpu::TextureUses::COLOR_TARGET,
            }),
        );
        let barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)
            .old_layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(self.family)
            .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
            .image(self.image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let device =
            unsafe { self.device.as_hal::<Api>() }.ok_or("Vulkan export device changed")?;
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("erika-external-release"),
            });
        unsafe {
            encoder.as_hal_mut::<Api, _, _>(|hal| {
                let hal = hal.ok_or("Vulkan export encoder changed")?;
                device.raw_device().cmd_pipeline_barrier(
                    hal.raw_handle(),
                    vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier],
                );
                Ok::<(), String>(())
            })
        }?;
        queue.submit([tracked.finish(), encoder.finish()]);
        Ok(())
    }
}
