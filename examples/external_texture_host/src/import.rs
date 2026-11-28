//! Import an owned liberika frame into the host's wgpu 26 device.
use anyhow::{Context, Result, bail};
use erika::{ExternalTextureFormat, ExternalTextureFrame, ExternalTextureKind};

pub fn import_frame(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    frame: ExternalTextureFrame,
) -> Result<wgpu::Texture> {
    if frame.info().format != ExternalTextureFormat::Rgba8Unorm {
        bail!("unsupported native texture format");
    }
    #[cfg(target_os = "linux")]
    return vulkan::import(device, queue, frame);
    #[cfg(target_os = "macos")]
    return metal_import(device, queue, frame);
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (device, queue, frame);
        bail!("this example supports Linux Vulkan and macOS Metal")
    }
}

fn descriptor(width: u32, height: u32) -> wgpu::TextureDescriptor<'static> {
    wgpu::TextureDescriptor {
        label: Some("erika-imported-host-frame"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    }
}

#[cfg(target_os = "macos")]
fn metal_import(
    device: &wgpu::Device,
    _queue: &wgpu::Queue,
    frame: ExternalTextureFrame,
) -> Result<wgpu::Texture> {
    use foreign_types::ForeignTypeRef;
    type Api = wgpu::hal::api::Metal;
    let info = *frame.info();
    if info.kind != ExternalTextureKind::Metal {
        bail!("expected a Metal frame");
    }
    let hal = unsafe { device.as_hal::<Api>() }.context("host must use Metal")?;
    if hal.raw_device().lock().registry_id() != info.device_id {
        bail!("host and producer selected different Metal GPUs");
    }
    // Retain the native texture independently before retiring the producer's
    // frame guard. The host HAL owns this reference through GPU retirement.
    let texture = unsafe { metal::TextureRef::from_ptr(info.native_handle as *mut _) }.to_owned();
    let desc = descriptor(info.width, info.height);
    let native = unsafe {
        wgpu::hal::metal::Device::texture_from_raw(
            texture,
            desc.format,
            metal::MTLTextureType::D2,
            1,
            1,
            wgpu::hal::CopyExtent {
                width: info.width,
                height: info.height,
                depth: 1,
            },
        )
    };
    drop(hal);
    let imported = unsafe { device.create_texture_from_hal::<Api>(native, &desc) };
    drop(frame);
    Ok(imported)
}

#[cfg(target_os = "linux")]
mod vulkan {
    use super::*;
    use ash::{khr, vk};
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};

    type Api = wgpu::hal::api::Vulkan;
    struct Allocation {
        raw: ash::Device,
        image: vk::Image,
        memory: vk::DeviceMemory,
        _frame: ExternalTextureFrame,
    }
    impl Drop for Allocation {
        fn drop(&mut self) {
            unsafe {
                self.raw.destroy_image(self.image, None);
                if self.memory != vk::DeviceMemory::null() {
                    self.raw.free_memory(self.memory, None);
                }
            }
        }
    }

    pub(super) fn import(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        frame: ExternalTextureFrame,
    ) -> Result<wgpu::Texture> {
        let info = *frame.info();
        if info.kind != ExternalTextureKind::VulkanOpaqueFd {
            bail!("expected a Vulkan OPAQUE_FD frame");
        }
        let hal = unsafe { device.as_hal::<Api>() }.context("host must use Vulkan")?;
        if !hal
            .enabled_device_extensions()
            .contains(&khr::external_memory_fd::NAME)
        {
            bail!("host device has not enabled VK_KHR_external_memory_fd");
        }
        let instance = hal.shared_instance().raw_instance();
        let physical = hal.raw_physical_device();
        let family = hal.queue_family_index();
        let raw = hal.raw_device().clone();
        let mut ids = vk::PhysicalDeviceIDProperties::default();
        let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut ids);
        unsafe { instance.get_physical_device_properties2(physical, &mut props) };
        if ids.device_uuid != info.device_uuid || ids.driver_uuid != info.driver_uuid {
            bail!(
                "host and producer selected different Vulkan GPUs/drivers; select the same GPU for both"
            );
        }
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let image = unsafe {
            raw.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::from_raw(info.vulkan_format as i32))
                    .extent(vk::Extent3D {
                        width: info.width,
                        height: info.height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::from_raw(info.vulkan_tiling as i32))
                    .usage(vk::ImageUsageFlags::from_raw(info.vulkan_usage))
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut external),
                None,
            )
        }
        .context("create imported image")?;
        let mut allocation = Allocation {
            raw: raw.clone(),
            image,
            memory: vk::DeviceMemory::null(),
            _frame: frame,
        };
        let required = unsafe { raw.get_image_memory_requirements(image) };
        if info.memory_type_index >= 32
            || required.memory_type_bits & (1 << info.memory_type_index) == 0
            || info.allocation_size < required.size
        {
            bail!("external allocation does not match the consumer's image requirements");
        }
        let fd = unsafe { libc::fcntl(info.native_handle as i32, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("duplicate external memory fd");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD)
            .fd(fd.as_raw_fd());
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        allocation.memory = unsafe {
            raw.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(info.allocation_size)
                    .memory_type_index(info.memory_type_index)
                    .push_next(&mut import)
                    .push_next(&mut dedicated),
                None,
            )
        }
        .context("import dedicated external allocation")?;
        let _ = fd.into_raw_fd(); // Vulkan consumed the duplicate on success.
        unsafe { raw.bind_image_memory(image, allocation.memory, 0) }
            .context("bind imported allocation")?;
        let desc = descriptor(info.width, info.height);
        let hal_desc = wgpu::hal::TextureDescriptor {
            label: desc.label,
            size: desc.size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: desc.dimension,
            format: desc.format,
            usage: wgpu::TextureUses::RESOURCE | wgpu::TextureUses::COPY_SRC,
            memory_flags: wgpu::hal::MemoryFlags::empty(),
            view_formats: vec![],
        };
        let mut native = unsafe {
            hal.texture_from_raw(image, &hal_desc, Some(Box::new(move || drop(allocation))))
        };
        // The pinned host HAL patch keeps core's first UNINITIALIZED transition
        // from discarding the imported image. An acquire is queued before use.
        unsafe { native.set_external_initial_usage(wgpu::TextureUses::RESOURCE) };
        drop(hal);
        let imported = unsafe { device.create_texture_from_hal::<Api>(native, &desc) };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("erika-host-acquire"),
        });
        let acquire = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
            .dst_queue_family_index(family)
            .dst_access_mask(vk::AccessFlags::SHADER_READ)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        unsafe {
            encoder.as_hal_mut::<Api, _, _>(|hal| {
                let hal = hal.context("Vulkan acquire encoder")?;
                raw.cmd_pipeline_barrier(
                    hal.raw_handle(),
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::FRAGMENT_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[acquire],
                );
                Ok::<_, anyhow::Error>(())
            })
        }?;
        let acquired = queue.submit(Some(encoder.finish()));
        // Raw HAL commands do not register texture references with wgpu's
        // submission tracker. Keep `imported` alive until this acquire finishes,
        // including when the caller drops it without ever drawing it.
        device
            .poll(wgpu::PollType::WaitForSubmissionIndex(acquired))
            .context("wait for imported image ownership")?;
        Ok(imported)
    }
    use std::os::fd::AsRawFd;
}
