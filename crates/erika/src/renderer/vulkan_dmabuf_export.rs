//! Linear, dma-buf backed export targets (Linux / Vulkan).
//!
//! [`crate::core::DmabufFrame`] hands the host a frame it can import without a
//! Vulkan device of its own: one linear RGBA8 image behind a `DMA_BUF`
//! descriptor, which GTK takes as a `GdkDmabufTexture` (`fourcc = ABGR8888`,
//! `modifier = 0`).
//!
//! The composite still renders into a fast (`OPTIMAL`) target; a single
//! same-device copy moves it here. Everything stays inside this renderer's
//! queue and device — no second Vulkan device, no ownership transfer from
//! `VK_QUEUE_FAMILY_EXTERNAL`.

use std::os::fd::{FromRawFd, OwnedFd};

use ash::vk;

use super::vulkan_export::{VulkanExportContext, memory_type_index};
use crate::core::DmabufFrame;

type Api = wgpu::hal::api::Vulkan;
const HANDLE_TYPE: vk::ExternalMemoryHandleTypeFlags =
    vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
const FORMAT: vk::Format = vk::Format::R8G8B8A8_UNORM;
const USAGE: vk::ImageUsageFlags = vk::ImageUsageFlags::from_raw(
    vk::ImageUsageFlags::TRANSFER_DST.as_raw() | vk::ImageUsageFlags::TRANSFER_SRC.as_raw(),
);

fn error(stage: &str, error: vk::Result) -> String {
    format!("dma-buf export {stage}: {error:?}")
}

/// The Vulkan objects behind one exported frame, owned by the wrapped
/// texture's drop guard.
struct Allocation {
    raw: ash::Device,
    image: vk::Image,
    memory: vk::DeviceMemory,
}

impl Drop for Allocation {
    fn drop(&mut self) {
        unsafe {
            self.raw.destroy_image(self.image, None);
            self.raw.free_memory(self.memory, None);
        }
    }
}

/// One linear dma-buf image a composite is copied into and the host imports.
///
/// Made per frame and dropped once [`Self::into_frame`] took the descriptor:
/// the dma-buf keeps the allocation itself alive while a consumer holds a
/// handle, so the image may go before the consumer is done.
pub(crate) struct DmabufExportTarget {
    /// The copy destination, wrapped so wgpu encodes and barriers it.
    pub texture: wgpu::Texture,
    device: wgpu::Device,
    frame: DmabufFrame,
    raw: ash::Device,
    image: vk::Image,
}

impl DmabufExportTarget {
    /// One linear dma-buf image to copy a composite into.
    ///
    /// Attempted rather than pre-checked: if the driver refuses, the error says
    /// which Vulkan call failed, followed by what the capability probe found.
    pub fn new(
        context: &VulkanExportContext,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        Self::make(context, device, width, height)
            .map_err(|error| format!("{error} [probe: {}]", context.dma_buf_note()))
    }

    fn make(
        context: &VulkanExportContext,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        context.check_size(width, height)?;
        let hal = unsafe { device.as_hal::<Api>() }.ok_or("dma-buf export requires Vulkan")?;
        let raw = context.raw_device().clone();

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
            // Linear is what a dma-buf consumer reads: rows of pixels, no
            // tile layout to describe.
            .tiling(vk::ImageTiling::LINEAR)
            .usage(USAGE)
            .sharing_mode(vk::SharingMode::EXCLUSIVE)
            .initial_layout(vk::ImageLayout::UNDEFINED)
            .push_next(&mut external_image);
        let image = unsafe { raw.create_image(&image_info, None) }
            .map_err(|e| error("image allocation", e))?;
        let mut allocation = Allocation {
            raw: raw.clone(),
            image,
            memory: vk::DeviceMemory::null(),
        };

        let requirements = unsafe { allocation.raw.get_image_memory_requirements(image) };
        let memory_type_index =
            memory_type_index(context.memory_properties(), requirements.memory_type_bits)
                .ok_or("dma-buf image has no compatible memory type")?;
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

        // The host imports by stride, not by a guessed `width * 4`: a driver
        // may pad the rows of a linear image.
        let layout = unsafe {
            allocation
                .raw
                .get_image_subresource_layout(image, color_subresource())
        };
        let fd_info = vk::MemoryGetFdInfoKHR::default()
            .memory(allocation.memory)
            .handle_type(HANDLE_TYPE);
        let fd = unsafe { context.fd_api().get_memory_fd(&fd_info) }
            .map_err(|e| error("export fd", e))?;
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let frame = DmabufFrame::new(
            fd,
            width,
            height,
            layout.row_pitch as u32,
            layout.offset as u32,
        );

        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let descriptor = wgpu::TextureDescriptor {
            label: Some("erika-dmabuf-frame"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        };
        let hal_descriptor = wgpu::hal::TextureDescriptor {
            label: descriptor.label,
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: descriptor.dimension,
            format: descriptor.format,
            usage: wgpu::TextureUses::COPY_DST | wgpu::TextureUses::COPY_SRC,
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
            device: device.clone(),
            frame,
            raw: raw.clone(),
            image,
        })
    }

    /// The command buffer that makes the copy's writes visible to whoever
    /// imports the descriptor. Submitted after the copy, so it runs in order.
    ///
    /// The consumer is another API (GTK's EGL import), not a Vulkan queue
    /// family, so no ownership transfer is involved.
    pub fn release_encoder(&self) -> Result<wgpu::CommandBuffer, String> {
        let barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
            .dst_access_mask(vk::AccessFlags::MEMORY_READ)
            .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
            .new_layout(vk::ImageLayout::GENERAL)
            .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
            .image(self.image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("erika-dmabuf-release"),
            });
        unsafe {
            encoder.as_hal_mut::<Api, _, _>(|hal| {
                let hal = hal.ok_or("Vulkan export encoder changed")?;
                self.raw.cmd_pipeline_barrier(
                    hal.raw_handle(),
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::ALL_COMMANDS,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[barrier],
                );
                Ok::<(), String>(())
            })
        }?;
        Ok(encoder.finish())
    }

    /// The descriptor this slot exports; hand out a `dup` of it.
    pub fn frame(&self) -> &DmabufFrame {
        &self.frame
    }
}

/// Ring of linear dma-buf targets a composite is copied into.
///
/// Two things this buys over allocating per frame (which is what a naive
/// implementation does): no `vkCreateImage` + `vkAllocateMemory` +
/// `vkGetMemoryFdKHR` per frame, and **no wait on the frame just submitted** —
/// the slot coming back around is waited on instead, so the CPU prepares frame
/// N+1 while the GPU still copies frame N.
///
/// Slots are handed out round-robin: a consumer must be done with a frame
/// before the same slot comes back ([`Self::SLOTS`] frames later), which is
/// what a compositor does (it paints within one frame).
pub(crate) struct DmabufExportPool {
    slots: Vec<DmabufSlot>,
    next: usize,
    width: u32,
    height: u32,
    /// The offscreen target the composite renders into, reused every frame.
    source: wgpu::Texture,
}

struct DmabufSlot {
    target: DmabufExportTarget,
    /// The copy that last wrote this slot; waited on before it is reused.
    pending: Option<wgpu::SubmissionIndex>,
}

impl DmabufExportPool {
    /// How many frames may be in flight.
    pub const SLOTS: usize = 3;

    pub fn new(
        context: &VulkanExportContext,
        device: &wgpu::Device,
        width: u32,
        height: u32,
    ) -> Result<Self, String> {
        let mut slots = Vec::with_capacity(Self::SLOTS);
        for _ in 0..Self::SLOTS {
            slots.push(DmabufSlot {
                target: DmabufExportTarget::new(context, device, width, height)?,
                pending: None,
            });
        }
        let source = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("erika-dmabuf-export-source"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        Ok(Self {
            slots,
            next: 0,
            width,
            height,
            source,
        })
    }

    /// The size it was made for; the caller rebuilds when the export size moves.
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The offscreen target the composite renders into (clone: cheap handle).
    pub fn source(&self) -> wgpu::Texture {
        self.source.clone()
    }

    /// Copies `source` into the next slot and hands the descriptor over.
    ///
    /// Returns without waiting for this copy: the fence is kept on the slot and
    /// waited on when that slot comes round again, [`Self::SLOTS`] frames later.
    pub fn present(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        source: &wgpu::Texture,
    ) -> Result<DmabufFrame, String> {
        let index = self.next;
        self.next = (self.next + 1) % self.slots.len();
        let slot = &mut self.slots[index];

        // The slot's previous copy must be done before it is written again.
        // Three frames old, so this normally returns at once.
        if let Some(pending) = slot.pending.take() {
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: Some(pending),
                    timeout: Some(std::time::Duration::from_secs(1)),
                })
                .map_err(|error| format!("dma-buf slot wait: {error}"))?;
        }

        let size = wgpu::Extent3d {
            width: self.width,
            height: self.height,
            depth_or_array_layers: 1,
        };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("erika-dmabuf-export"),
        });
        encoder.copy_texture_to_texture(
            source.as_image_copy(),
            slot.target.texture.as_image_copy(),
            size,
        );
        let copy = encoder.finish();
        // The release barrier must follow the copy in the same submission, and
        // wgpu 29 forbids mixing its regular and raw encoding APIs on one
        // encoder: two command buffers, submitted together.
        let release = slot.target.release_encoder()?;
        let submission = queue.submit([copy, release]);
        slot.pending = Some(submission);

        // A fresh descriptor per handoff: the consumer closes it when done, and
        // the slot's own descriptor stays with the slot.
        let fd = slot
            .target
            .frame()
            .try_clone_fd()
            .map_err(|error| format!("dma-buf dup: {error}"))?;
        Ok(DmabufFrame::new(
            fd,
            self.width,
            self.height,
            slot.target.frame().stride(),
            slot.target.frame().offset(),
        ))
    }
}

fn color_subresource() -> vk::ImageSubresource {
    vk::ImageSubresource {
        aspect_mask: vk::ImageAspectFlags::COLOR,
        mip_level: 0,
        array_layer: 0,
    }
}
