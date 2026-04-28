//! Metal GPU implementation of the literal pre-filter.
//! Only compiled on macOS (`cfg(target_os = "macos")`).

use metal::*;

const SHADER_SRC: &str = include_str!("shader.metal");

#[repr(C)]
#[derive(Copy, Clone)]
struct LineEntry {
    offset: u32,
    length: u32,
}

pub struct MetalVerifier {
    device: Device,
    pipeline: ComputePipelineState,
    queue: CommandQueue,
}

impl MetalVerifier {
    pub fn new() -> Option<Self> {
        let device = Device::system_default()?;
        let options = CompileOptions::new();
        let library = device
            .new_library_with_source(SHADER_SRC, &options)
            .ok()?;
        let func = library.get_function("literal_search", None).ok()?;
        let pipeline = device
            .new_compute_pipeline_state_with_function(&func)
            .ok()?;
        let queue = device.new_command_queue();
        Some(Self { device, pipeline, queue })
    }

    /// Filter `line_slices` by literal `needle` using the GPU.
    /// Returns a bitmask: `result[i] = true` iff line i contains needle.
    ///
    /// `line_slices`: Vec of (offset_into_data, length) — indices into `line_data`.
    /// `line_data`: all candidate line bytes packed contiguously.
    pub fn filter(
        &self,
        line_data: &[u8],
        line_slices: &[(u32, u32)],  // (offset, len)
        needle: &[u8],
    ) -> Vec<bool> {
        let n = line_slices.len();
        if n == 0 { return vec![]; }

        // --- Build GPU buffers ---
        let data_buf = self.device.new_buffer_with_data(
            line_data.as_ptr() as *const _,
            line_data.len() as u64,
            MTLResourceOptions::StorageModeShared,
        );

        let entries: Vec<LineEntry> = line_slices.iter()
            .map(|&(off, len)| LineEntry { offset: off, length: len })
            .collect();
        let entries_buf = self.device.new_buffer_with_data(
            entries.as_ptr() as *const _,
            (entries.len() * std::mem::size_of::<LineEntry>()) as u64,
            MTLResourceOptions::StorageModeShared,
        );

        let needle_buf = self.device.new_buffer_with_data(
            needle.as_ptr() as *const _,
            needle.len() as u64,
            MTLResourceOptions::StorageModeShared,
        );

        let needle_len = needle.len() as u32;
        let needle_len_buf = self.device.new_buffer_with_data(
            &needle_len as *const u32 as *const _,
            std::mem::size_of::<u32>() as u64,
            MTLResourceOptions::StorageModeShared,
        );

        let results_buf = self.device.new_buffer(
            (n * std::mem::size_of::<u32>()) as u64,
            MTLResourceOptions::StorageModeShared,
        );

        // --- Dispatch compute ---
        let cmd_buf = self.queue.new_command_buffer();
        let encoder = cmd_buf.new_compute_command_encoder();

        encoder.set_compute_pipeline_state(&self.pipeline);
        encoder.set_buffer(0, Some(&data_buf), 0);
        encoder.set_buffer(1, Some(&entries_buf), 0);
        encoder.set_buffer(2, Some(&needle_buf), 0);
        encoder.set_buffer(3, Some(&needle_len_buf), 0);
        encoder.set_buffer(4, Some(&results_buf), 0);

        let thread_group_size = MTLSize {
            width: self.pipeline.max_total_threads_per_threadgroup().min(256),
            height: 1,
            depth: 1,
        };
        let grid_size = MTLSize {
            width: n as u64,
            height: 1,
            depth: 1,
        };

        encoder.dispatch_threads(grid_size, thread_group_size);
        encoder.end_encoding();
        cmd_buf.commit();
        cmd_buf.wait_until_completed();

        // --- Read results ---
        let ptr = results_buf.contents() as *const u32;
        (0..n).map(|i| unsafe { *ptr.add(i) } != 0).collect()
    }
}

/// Lazy singleton — init once, reuse across calls.
static VERIFIER: std::sync::OnceLock<Option<MetalVerifier>> = std::sync::OnceLock::new();

pub fn global_verifier() -> Option<&'static MetalVerifier> {
    VERIFIER.get_or_init(MetalVerifier::new).as_ref()
}
