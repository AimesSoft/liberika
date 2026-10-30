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

/// Device constants for export, queried once per renderer device.
pub(crate) struct VulkanExportContext {
    raw: ash::Device,
    fd_api: khr::external_memory_fd::Device,
    family: u32,
    max_extent: vk::Extent3D,
    memory_properties: vk::PhysicalDeviceMemoryProperties,
    device_uuid: [u8; 16],
    driver_uuid: [u8; 16],
}

fn error(stage: &str, error: vk::Result) -> String {
    format!("Vulkan external texture {stage}: {error:?}")
}

/// Picks plain device-local memory for the exported image. Host-visible
/// device-local types are the small BAR heap without resizable BAR, and AMD
/// device-coherent, protected and lazily allocated types need features or
/// usage this renderer does not enable. Software and unified-memory devices
/// that only offer host-visible types use the first allowed one.
fn memory_type_index(
    properties: &vk::PhysicalDeviceMemoryProperties,
    type_bits: u32,
) -> Option<u32> {
    let unsupported = vk::MemoryPropertyFlags::DEVICE_COHERENT_AMD
        | vk::MemoryPropertyFlags::DEVICE_UNCACHED_AMD
        | vk::MemoryPropertyFlags::PROTECTED
        | vk::MemoryPropertyFlags::LAZILY_ALLOCATED;
    let allowed = (0..properties.memory_type_count)
        .filter(move |index| type_bits & (1 << index) != 0)
        .map(|index| {
            (
                index,
                properties.memory_types[index as usize].property_flags,
            )
        })
        .filter(move |(_, flags)| !flags.intersects(unsupported));
    let device_local =
        |flags: &vk::MemoryPropertyFlags| flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL);
    allowed
        .clone()
        .find(|(_, flags)| {
            device_local(flags) && !flags.contains(vk::MemoryPropertyFlags::HOST_VISIBLE)
        })
        .or_else(|| allowed.clone().find(|(_, flags)| device_local(flags)))
        .or_else(|| allowed.clone().next())
        .map(|(index, _)| index)
}

impl VulkanExportContext {
    pub fn new(device: &wgpu::Device) -> Result<Self, String> {
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
        let max_extent = properties.image_format_properties.max_extent;
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
        Ok(Self {
            fd_api: khr::external_memory_fd::Device::new(instance, &raw),
            raw,
            family: hal.queue_family_index(),
            max_extent,
            memory_properties,
            device_uuid: identity.device_uuid,
            driver_uuid: identity.driver_uuid,
        })
    }

    pub fn check_size(&self, width: u32, height: u32) -> Result<(), String> {
        if width > self.max_extent.width || height > self.max_extent.height {
            return Err("external texture exceeds the Vulkan format extent limit".into());
        }
        Ok(())
    }
}

impl VulkanExportTarget {
    pub fn new(
        context: &VulkanExportContext,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        context.check_size(width, height)?;
        let hal =
            unsafe { device.as_hal::<Api>() }.ok_or("native texture export requires Vulkan")?;
        let raw = context.raw.clone();
        let family = context.family;
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
        let memory_type_index =
            memory_type_index(&context.memory_properties, requirements.memory_type_bits)
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
        let fd =
            unsafe { context.fd_api.get_memory_fd(&fd_info) }.map_err(|e| error("export fd", e))?;
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
            device_uuid: context.device_uuid,
            driver_uuid: context.driver_uuid,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn properties(types: &[vk::MemoryPropertyFlags]) -> vk::PhysicalDeviceMemoryProperties {
        let mut properties = vk::PhysicalDeviceMemoryProperties {
            memory_type_count: types.len() as u32,
            ..Default::default()
        };
        for (slot, flags) in properties.memory_types.iter_mut().zip(types) {
            slot.property_flags = *flags;
        }
        properties
    }

    #[test]
    fn export_memory_prefers_plain_device_local_types() {
        use vk::MemoryPropertyFlags as F;
        // RADV without resizable BAR: VRAM, GTT, the BAR heap, cached GTT,
        // then the AMD device-coherent variants.
        let radv = properties(&[
            F::DEVICE_LOCAL,
            F::HOST_VISIBLE | F::HOST_COHERENT,
            F::DEVICE_LOCAL | F::HOST_VISIBLE | F::HOST_COHERENT,
            F::HOST_VISIBLE | F::HOST_COHERENT | F::HOST_CACHED,
            F::DEVICE_LOCAL | F::DEVICE_COHERENT_AMD | F::DEVICE_UNCACHED_AMD,
            F::DEVICE_LOCAL
                | F::HOST_VISIBLE
                | F::HOST_COHERENT
                | F::DEVICE_COHERENT_AMD
                | F::DEVICE_UNCACHED_AMD,
        ]);
        assert_eq!(memory_type_index(&radv, 0b11_0101), Some(0));
        assert_eq!(memory_type_index(&radv, 0b11_0100), Some(2));
        assert_eq!(memory_type_index(&radv, 0b11_1000), Some(3));
        assert_eq!(memory_type_index(&radv, 0b11_0000), None);
        assert_eq!(memory_type_index(&radv, 0), None);

        // llvmpipe exposes a single host-visible type.
        let software =
            properties(&[F::DEVICE_LOCAL | F::HOST_VISIBLE | F::HOST_COHERENT | F::HOST_CACHED]);
        assert_eq!(memory_type_index(&software, 0b1), Some(0));
    }
}
