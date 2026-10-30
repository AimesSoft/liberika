//! Independent consumers. Readback is test instrumentation, never the export
//! path. Linux imports into a separate instance/device and acquires ownership.
use crate::core::ExternalTextureFrame;

#[cfg(any(target_os = "macos", target_os = "ios", target_os = "tvos"))]
pub(crate) fn read_pixels(frame: &ExternalTextureFrame) -> Vec<u8> {
    use objc2::runtime::ProtocolObject;
    use objc2_metal::{
        MTLBlitCommandEncoder, MTLBuffer, MTLCommandBuffer, MTLCommandBufferStatus,
        MTLCommandEncoder, MTLCommandQueue, MTLDevice, MTLOrigin, MTLResource, MTLResourceOptions,
        MTLSize, MTLTexture,
    };
    let info = frame.info();
    assert_eq!(info.kind, crate::ExternalTextureKind::Metal);
    let texture = unsafe { &*(info.native_handle as *const ProtocolObject<dyn MTLTexture>) };
    let device = texture.device();
    assert_eq!(device.registryID(), info.device_id);
    let queue = device.newCommandQueue().unwrap();
    let command = queue.commandBuffer().unwrap();
    let row_bytes = info.width as usize * 4;
    let length = row_bytes * info.height as usize;
    let buffer = device
        .newBufferWithLength_options(length, MTLResourceOptions::StorageModeShared)
        .unwrap();
    let blit = command.blitCommandEncoder().unwrap();
    unsafe {
        blit.copyFromTexture_sourceSlice_sourceLevel_sourceOrigin_sourceSize_toBuffer_destinationOffset_destinationBytesPerRow_destinationBytesPerImage(
            texture, 0, 0, MTLOrigin { x: 0, y: 0, z: 0 },
            MTLSize { width: info.width as usize, height: info.height as usize, depth: 1 },
            &buffer, 0, row_bytes, length);
    }
    blit.endEncoding();
    command.commit();
    command.waitUntilCompleted();
    assert_eq!(command.status(), MTLCommandBufferStatus::Completed);
    unsafe { std::slice::from_raw_parts(buffer.contents().as_ptr().cast::<u8>(), length).to_vec() }
}

#[cfg(all(target_os = "linux", not(target_env = "ohos")))]
pub(crate) fn read_pixels(frame: &ExternalTextureFrame) -> Vec<u8> {
    use ash::{khr, vk};
    let info = frame.info();
    assert_eq!(info.kind, crate::ExternalTextureKind::VulkanOpaqueFd);
    unsafe {
        let entry = ash::Entry::load().unwrap();
        let app = vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2);
        let instance = entry
            .create_instance(
                &vk::InstanceCreateInfo::default().application_info(&app),
                None,
            )
            .unwrap();
        let physical = instance
            .enumerate_physical_devices()
            .unwrap()
            .into_iter()
            .find(|&physical| {
                let mut ids = vk::PhysicalDeviceIDProperties::default();
                let mut props = vk::PhysicalDeviceProperties2::default().push_next(&mut ids);
                instance.get_physical_device_properties2(physical, &mut props);
                ids.device_uuid == info.device_uuid && ids.driver_uuid == info.driver_uuid
            })
            .expect("consumer must match the exporting GPU and driver");
        let families = instance.get_physical_device_queue_family_properties(physical);
        let family = families
            .iter()
            .position(|props| props.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .unwrap() as u32;
        let priorities = [1.0];
        let queues = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(family)
            .queue_priorities(&priorities)];
        let extensions = [khr::external_memory_fd::NAME.as_ptr()];
        let device = instance
            .create_device(
                physical,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&queues)
                    .enabled_extension_names(&extensions),
                None,
            )
            .unwrap();
        let queue = device.get_device_queue(family, 0);
        let mut external = vk::ExternalMemoryImageCreateInfo::default()
            .handle_types(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD);
        let image = device
            .create_image(
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
            .unwrap();
        let required = device.get_image_memory_requirements(image);
        assert_ne!(required.memory_type_bits & (1 << info.memory_type_index), 0);
        assert!(info.allocation_size >= required.size);
        // Import consumes this duplicate on success. The original stays owned
        // by the frame and must remain valid until frame_release/drop.
        let fd = libc::fcntl(info.native_handle as i32, libc::F_DUPFD_CLOEXEC, 0);
        assert!(fd >= 0);
        let mut import = vk::ImportMemoryFdInfoKHR::default()
            .handle_type(vk::ExternalMemoryHandleTypeFlags::OPAQUE_FD)
            .fd(fd);
        let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
        let memory = device
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(info.allocation_size)
                    .memory_type_index(info.memory_type_index)
                    .push_next(&mut import)
                    .push_next(&mut dedicated),
                None,
            )
            .unwrap();
        device.bind_image_memory(image, memory, 0).unwrap();
        let length = info.width as usize * info.height as usize * 4;
        let buffer = device
            .create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(length as u64)
                    .usage(vk::BufferUsageFlags::TRANSFER_DST)
                    .sharing_mode(vk::SharingMode::EXCLUSIVE),
                None,
            )
            .unwrap();
        let required = device.get_buffer_memory_requirements(buffer);
        let properties = instance.get_physical_device_memory_properties(physical);
        let host_type = (0..properties.memory_type_count)
            .find(|&index| {
                required.memory_type_bits & (1 << index) != 0
                    && properties.memory_types[index as usize]
                        .property_flags
                        .contains(
                            vk::MemoryPropertyFlags::HOST_VISIBLE
                                | vk::MemoryPropertyFlags::HOST_COHERENT,
                        )
            })
            .unwrap();
        let host_memory = device
            .allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(required.size)
                    .memory_type_index(host_type),
                None,
            )
            .unwrap();
        device.bind_buffer_memory(buffer, host_memory, 0).unwrap();
        let pool = device
            .create_command_pool(
                &vk::CommandPoolCreateInfo::default().queue_family_index(family),
                None,
            )
            .unwrap();
        let command = device
            .allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
            .unwrap()[0];
        device
            .begin_command_buffer(command, &vk::CommandBufferBeginInfo::default())
            .unwrap();
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let acquire = vk::ImageMemoryBarrier::default()
            .old_layout(vk::ImageLayout::GENERAL)
            .new_layout(vk::ImageLayout::TRANSFER_SRC_OPTIMAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
            .dst_queue_family_index(family)
            .dst_access_mask(vk::AccessFlags::TRANSFER_READ)
            .image(image)
            .subresource_range(range);
        device.cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::TOP_OF_PIPE,
            vk::PipelineStageFlags::TRANSFER,
            vk::DependencyFlags::empty(),
            &[],
            &[],
            &[acquire],
        );
        let region = vk::BufferImageCopy::default()
            .image_subresource(vk::ImageSubresourceLayers {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                mip_level: 0,
                base_array_layer: 0,
                layer_count: 1,
            })
            .image_extent(vk::Extent3D {
                width: info.width,
                height: info.height,
                depth: 1,
            });
        device.cmd_copy_image_to_buffer(
            command,
            image,
            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
            buffer,
            &[region],
        );
        let host_barrier = vk::BufferMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::HOST_READ)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .buffer(buffer)
            .size(vk::WHOLE_SIZE);
        device.cmd_pipeline_barrier(
            command,
            vk::PipelineStageFlags::TRANSFER,
            vk::PipelineStageFlags::HOST,
            vk::DependencyFlags::empty(),
            &[],
            &[host_barrier],
            &[],
        );
        device.end_command_buffer(command).unwrap();
        let fence = device
            .create_fence(&vk::FenceCreateInfo::default(), None)
            .unwrap();
        device
            .queue_submit(
                queue,
                &[vk::SubmitInfo::default().command_buffers(&[command])],
                fence,
            )
            .unwrap();
        device
            .wait_for_fences(&[fence], true, 5_000_000_000)
            .unwrap();
        let mapped = device
            .map_memory(host_memory, 0, length as u64, vk::MemoryMapFlags::empty())
            .unwrap();
        let pixels = std::slice::from_raw_parts(mapped.cast::<u8>(), length).to_vec();
        device.unmap_memory(host_memory);
        device.destroy_fence(fence, None);
        device.destroy_command_pool(pool, None);
        device.destroy_buffer(buffer, None);
        device.free_memory(host_memory, None);
        device.destroy_image(image, None);
        device.free_memory(memory, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
        pixels
    }
}
