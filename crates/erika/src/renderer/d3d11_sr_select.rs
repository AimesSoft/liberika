//! Chooses the fastest D3D11-side executor for a network.
//!
//! The scalar compute engine ([`D3d11SrEngine`]) always works. Matrix-unit
//! accelerators (Vulkan cooperative matrices, D3D12 Shader Model 6.10
//! `linalg`) run next to it through shared textures and fences. Which one is
//! fastest depends on the GPU and the network: Tensor Cores win easily on
//! wide networks such as ArtCNN R8F64 but can lose to well-scheduled FMA code
//! on 16-channel layers. So every candidate runs a few frames whenever the
//! network or frame size changes, and the one with the lowest measured GPU
//! time keeps running. `ERIKA_SR_BACKEND=scalar|coopmat|linalg` pins a
//! choice.

use std::sync::Arc;
use std::time::Duration;

use ::windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11ShaderResourceView,
};

use crate::core::{LumaUpscalerBackendStatus, PlayerError, Result};
use crate::renderer::d3d11_sr::{D3d11SrEngine, UpscaledLuma};
use crate::renderer::pipeline::LumaUpscalerMode;
use crate::renderer::sr::model::SrModel;
use crate::renderer::sr::reference::LumaRange;

/// Frames each candidate runs while calibrating; the first ones warm up
/// pipelines and caches and are not measured.
const CALIBRATION_FRAMES: usize = 8;
const CALIBRATION_WARMUP: usize = 3;

/// A matrix-unit executor running beside the D3D11 renderer. Implementations
/// share the scalar engine's contract: the returned view is a `(2w)x(2h)`
/// `R16_FLOAT` luma texture with `sr::reference::run` semantics, valid until
/// the next `encode`.
pub(crate) trait SrAccelerator: Send {
    fn label(&self) -> &'static str;
    fn status(&self) -> LumaUpscalerBackendStatus;
    fn supports(&self, model: &SrModel) -> bool;
    fn prepare(&mut self, model: Arc<SrModel>) -> Result<()>;
    fn encode(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        luma: &ID3D11ShaderResourceView,
        width: u32,
        height: u32,
        range: LumaRange,
    ) -> Result<ID3D11ShaderResourceView>;
    fn last_gpu_duration(&self) -> Option<Duration>;
}

/// Which executor produced (or will produce) frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Choice {
    Scalar,
    Accelerator(usize),
}

struct Calibration {
    candidates: Vec<Choice>,
    current: usize,
    frames: usize,
    samples: Vec<Vec<f64>>,
}

/// Creates the accelerators available for a device. Failures are reasons to
/// report, not errors. Runs on a background thread.
pub(crate) type AcceleratorFactory =
    fn(&ID3D11Device) -> Vec<std::result::Result<Box<dyn SrAccelerator>, String>>;

/// Accelerators prepared for one model, produced off the render thread.
struct PreparedSet {
    model: Arc<SrModel>,
    accelerators: Vec<Box<dyn SrAccelerator>>,
    accepted: Vec<bool>,
    unavailable: Vec<String>,
}

pub(crate) struct D3d11Upscaler {
    scalar: D3d11SrEngine,
    factory: AcceleratorFactory,
    accelerators: Option<Vec<Box<dyn SrAccelerator>>>,
    unavailable: Vec<String>,
    /// Model the accelerators were prepared for, and which accepted it.
    prepared: Option<(Arc<SrModel>, Vec<bool>)>,
    /// Background creation/preparation; the accelerators are moved into it.
    pending: Option<std::sync::mpsc::Receiver<PreparedSet>>,
    created: bool,
    choice: Choice,
    /// (model, width, height) the current choice was measured for.
    calibrated_for: Option<(usize, u32, u32)>,
    calibration: Option<Calibration>,
    cached: Option<(u64, u32, u32, LumaRange, ID3D11ShaderResourceView)>,
    accelerated_frames: u64,
    accelerator_failures: u64,
    last_accelerator_gpu: Duration,
}

impl D3d11Upscaler {
    pub(crate) fn new(factory: AcceleratorFactory) -> Self {
        Self {
            scalar: D3d11SrEngine::default(),
            factory,
            accelerators: None,
            unavailable: Vec::new(),
            prepared: None,
            pending: None,
            created: false,
            choice: Choice::Scalar,
            calibrated_for: None,
            calibration: None,
            cached: None,
            accelerated_frames: 0,
            accelerator_failures: 0,
            last_accelerator_gpu: Duration::ZERO,
        }
    }

    pub(crate) fn status(&self) -> LumaUpscalerBackendStatus {
        match (self.choice, self.accelerators.as_ref()) {
            (Choice::Accelerator(index), Some(accelerators)) if self.scalar.is_active() => {
                accelerators[index].status()
            }
            _ => self.scalar.status(),
        }
    }

    pub(crate) fn fallback_count(&self) -> u64 {
        self.scalar.fallback_count() + self.accelerator_failures
    }

    pub(crate) fn upscaled_frames(&self) -> u64 {
        self.scalar.upscaled_frames() + self.accelerated_frames
    }

    pub(crate) fn last_encode_duration(&self) -> Duration {
        self.scalar.last_encode_duration()
    }

    pub(crate) fn last_gpu_duration(&self) -> Duration {
        match self.choice {
            Choice::Scalar => self.scalar.last_gpu_duration(),
            Choice::Accelerator(_) => self.last_accelerator_gpu,
        }
    }

    pub(crate) fn allocated_bytes(&self) -> u64 {
        self.scalar.allocated_bytes()
    }

    /// Engine label for diagnostics.
    pub(crate) fn engine(&self) -> &'static str {
        match (self.choice, self.accelerators.as_ref()) {
            (Choice::Accelerator(index), Some(accelerators)) => accelerators[index].label(),
            _ => "scalar_compute",
        }
    }

    /// Why the network is inactive, or why it runs without a matrix unit.
    pub(crate) fn reason(&self) -> Option<String> {
        if let Some(failure) = self.scalar.last_failure() {
            return Some(failure.to_string());
        }
        (!self.unavailable.is_empty() && self.choice == Choice::Scalar).then(|| {
            format!(
                "matrix accelerators unavailable: {}",
                self.unavailable.join("; ")
            )
        })
    }

    pub(crate) fn attach_device(
        &mut self,
        device: &ID3D11Device,
        mode: LumaUpscalerMode,
    ) -> Result<()> {
        // A pending job holds accelerators of the old device; drop it.
        self.pending = None;
        self.accelerators = None;
        self.created = false;
        self.unavailable.clear();
        self.reset_selection();
        self.scalar.attach_device(device, mode)
    }

    pub(crate) fn set_mode(&mut self, device: &ID3D11Device, mode: LumaUpscalerMode) -> Result<()> {
        self.reset_selection();
        self.scalar.set_mode(device, mode)
    }

    pub(crate) fn store_custom_model(&mut self, model: Option<Arc<SrModel>>) {
        self.scalar.store_custom_model(model);
    }

    pub(crate) fn set_custom_model(&mut self, device: &ID3D11Device, model: Option<Arc<SrModel>>) {
        self.reset_selection();
        self.scalar.set_custom_model(device, model);
    }

    pub(crate) fn poll(&mut self, device: &ID3D11Device) {
        self.scalar.poll(device);
    }

    pub(crate) fn record_runtime_failure(&mut self, error: &PlayerError) {
        self.reset_selection();
        self.scalar.record_runtime_failure(error);
    }

    fn reset_selection(&mut self) {
        self.prepared = None;
        self.choice = Choice::Scalar;
        self.calibrated_for = None;
        self.calibration = None;
        self.cached = None;
    }

    pub(crate) fn encode(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        luma: &ID3D11ShaderResourceView,
        width: u32,
        height: u32,
        range: LumaRange,
        frame_token: Option<u64>,
    ) -> Result<Option<UpscaledLuma>> {
        self.scalar.poll(device);
        let Some(model) = self.scalar.active_model() else {
            return self
                .scalar
                .encode(device, context, luma, width, height, range, frame_token);
        };
        self.prepare_accelerators(device, &model);
        let key = (Arc::as_ptr(&model) as usize, width, height);
        let ready = self
            .prepared
            .as_ref()
            .is_some_and(|(prepared, _)| Arc::ptr_eq(prepared, &model));
        if !ready {
            // Accelerators are still being prepared off-thread.
            self.choice = Choice::Scalar;
            return self
                .scalar
                .encode(device, context, luma, width, height, range, frame_token);
        }
        if self.calibrated_for != Some(key) && self.calibration.is_none() {
            self.start_calibration(key);
        }
        let choice = self
            .calibration
            .as_ref()
            .map_or(self.choice, |calibration| {
                calibration.candidates[calibration.current]
            });

        let output = match choice {
            Choice::Scalar => {
                self.scalar
                    .encode(device, context, luma, width, height, range, frame_token)?
            }
            Choice::Accelerator(index) => {
                if let (Some(token), Some((cached_token, w, h, r, view))) =
                    (frame_token, self.cached.as_ref())
                {
                    if token == *cached_token && (*w, *h, *r) == (width, height, range) {
                        return Ok(Some(UpscaledLuma {
                            view: view.clone(),
                            width: width * 2,
                            height: height * 2,
                        }));
                    }
                }
                let accelerator = &mut self.accelerators.as_mut().expect("prepared")[index];
                match accelerator.encode(device, context, luma, width, height, range) {
                    Ok(view) => {
                        self.accelerated_frames = self.accelerated_frames.saturating_add(1);
                        if let Some(duration) = accelerator.last_gpu_duration() {
                            self.last_accelerator_gpu = duration;
                        }
                        if let Some(token) = frame_token {
                            self.cached = Some((token, width, height, range, view.clone()));
                        }
                        Some(UpscaledLuma {
                            view,
                            width: width * 2,
                            height: height * 2,
                        })
                    }
                    Err(error) => {
                        // Drop this accelerator for the model and fall back.
                        crate::trace::diagnostic_at(
                            crate::trace::LogLevel::Warn,
                            format!(
                                "Erika SR accelerator {} failed, using scalar compute: {error}",
                                accelerator.label()
                            ),
                        );
                        self.accelerator_failures = self.accelerator_failures.saturating_add(1);
                        if let Some((_, accepted)) = self.prepared.as_mut() {
                            accepted[index] = false;
                        }
                        self.calibration = None;
                        self.calibrated_for = Some(key);
                        self.choice = Choice::Scalar;
                        self.scalar.encode(
                            device,
                            context,
                            luma,
                            width,
                            height,
                            range,
                            frame_token,
                        )?
                    }
                }
            }
        };
        if output.is_some() {
            self.advance_calibration(choice, key);
        }
        Ok(output)
    }

    /// Creates (once per device) and prepares the accelerators for `model` on
    /// a background thread: Vulkan device creation and pipeline builds take
    /// long enough to stutter playback. Scalar compute runs meanwhile.
    fn prepare_accelerators(&mut self, device: &ID3D11Device, model: &Arc<SrModel>) {
        if let Some(receiver) = self.pending.as_ref() {
            match receiver.try_recv() {
                Ok(set) => {
                    self.pending = None;
                    self.unavailable.extend(set.unavailable);
                    self.accelerators = Some(set.accelerators);
                    self.prepared = Some((set.model, set.accepted));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => return,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.unavailable
                        .push("accelerator setup thread exited".to_string());
                    self.prepared = Some((model.clone(), Vec::new()));
                }
            }
        }
        if self
            .prepared
            .as_ref()
            .is_some_and(|(prepared, _)| Arc::ptr_eq(prepared, model))
        {
            return;
        }
        if std::env::var("ERIKA_SR_BACKEND").as_deref() == Ok("scalar") {
            self.prepared = Some((model.clone(), Vec::new()));
            return;
        }
        let existing = self.accelerators.take();
        let create = !self.created;
        self.created = true;
        let factory = self.factory;
        let device = device.clone();
        let job_model = model.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("erika-sr-accelerators".to_string())
            .spawn(move || {
                let mut unavailable = Vec::new();
                let mut accelerators = existing.unwrap_or_default();
                if create {
                    for candidate in factory(&device) {
                        match candidate {
                            Ok(accelerator) => accelerators.push(accelerator),
                            Err(reason) => unavailable.push(reason),
                        }
                    }
                }
                let accepted = accelerators
                    .iter_mut()
                    .map(|accelerator| {
                        if !accelerator.supports(&job_model) {
                            return false;
                        }
                        match accelerator.prepare(job_model.clone()) {
                            Ok(()) => true,
                            Err(error) => {
                                unavailable.push(format!("{}: {error}", accelerator.label()));
                                false
                            }
                        }
                    })
                    .collect();
                let _ = sender.send(PreparedSet {
                    model: job_model,
                    accelerators,
                    accepted,
                    unavailable,
                });
            });
        match spawned {
            Ok(_) => self.pending = Some(receiver),
            Err(error) => {
                self.unavailable
                    .push(format!("cannot spawn accelerator setup: {error}"));
                self.prepared = Some((model.clone(), Vec::new()));
            }
        }
    }

    fn start_calibration(&mut self, key: (usize, u32, u32)) {
        let accepted = self
            .prepared
            .as_ref()
            .map(|(_, accepted)| accepted.clone())
            .unwrap_or_default();
        let labels: Vec<&'static str> = self
            .accelerators
            .as_ref()
            .map(|accelerators| {
                accelerators
                    .iter()
                    .map(|accelerator| accelerator.label())
                    .collect()
            })
            .unwrap_or_default();
        let pinned = std::env::var("ERIKA_SR_BACKEND").ok();
        let mut candidates = vec![Choice::Scalar];
        for (index, accepted) in accepted.iter().enumerate() {
            if *accepted {
                candidates.push(Choice::Accelerator(index));
            }
        }
        let pinned = pinned.map(|pinned| match pinned.as_str() {
            "coopmat" => "cooperative_matrix".to_string(),
            "linalg" => "linalg_matrix".to_string(),
            _ => pinned,
        });
        if let Some(pinned) = pinned.as_deref() {
            if let Some(index) = labels.iter().position(|label| *label == pinned) {
                if accepted.get(index).copied().unwrap_or(false) {
                    self.choice = Choice::Accelerator(index);
                    self.calibrated_for = Some(key);
                    return;
                }
            }
        }
        if candidates.len() == 1 {
            self.choice = Choice::Scalar;
            self.calibrated_for = Some(key);
            return;
        }
        self.calibration = Some(Calibration {
            samples: vec![Vec::new(); candidates.len()],
            candidates,
            current: 0,
            frames: 0,
        });
    }

    fn advance_calibration(&mut self, ran: Choice, key: (usize, u32, u32)) {
        let gpu = match ran {
            Choice::Scalar => Some(self.scalar.last_gpu_duration()),
            Choice::Accelerator(index) => self
                .accelerators
                .as_ref()
                .and_then(|accelerators| accelerators[index].last_gpu_duration()),
        };
        let Some(calibration) = self.calibration.as_mut() else {
            return;
        };
        calibration.frames += 1;
        if calibration.frames > CALIBRATION_WARMUP {
            if let Some(gpu) = gpu.filter(|gpu| !gpu.is_zero()) {
                calibration.samples[calibration.current].push(gpu.as_secs_f64() * 1000.0);
            }
        }
        if calibration.frames < CALIBRATION_FRAMES {
            return;
        }
        calibration.current += 1;
        calibration.frames = 0;
        if calibration.current < calibration.candidates.len() {
            return;
        }
        let calibration = self.calibration.take().expect("checked");
        let medians: Vec<Option<f64>> = calibration
            .samples
            .iter()
            .map(|samples| {
                let mut samples = samples.clone();
                samples.sort_by(f64::total_cmp);
                samples.get(samples.len() / 2).copied()
            })
            .collect();
        let best = medians
            .iter()
            .enumerate()
            .filter_map(|(index, median)| median.map(|median| (index, median)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map_or(0, |(index, _)| index);
        self.choice = calibration.candidates[best];
        self.calibrated_for = Some(key);
        self.cached = None;
        let label = |choice: Choice| match choice {
            Choice::Scalar => "scalar",
            Choice::Accelerator(index) => self
                .accelerators
                .as_ref()
                .map_or("accelerator", |accelerators| accelerators[index].label()),
        };
        let summary: Vec<String> = calibration
            .candidates
            .iter()
            .zip(&medians)
            .map(|(choice, median)| match median {
                Some(median) => format!("{} {median:.2} ms", label(*choice)),
                None => format!("{} unmeasured", label(*choice)),
            })
            .collect();
        crate::trace::diagnostic(
            serde_json::json!({
                "event": "luma_upscaler_backend_selection",
                "renderer": "d3d11",
                "width": key.1,
                "height": key.2,
                "candidates": summary,
                "selected": label(self.choice),
                "unavailable": self.unavailable,
            })
            .to_string(),
        );
    }
}

impl SrAccelerator for crate::renderer::d3d11_vk_coopmat::VkCoopMatUpscaler {
    fn label(&self) -> &'static str {
        "cooperative_matrix"
    }

    fn status(&self) -> LumaUpscalerBackendStatus {
        LumaUpscalerBackendStatus::CooperativeMatrix
    }

    fn supports(&self, model: &SrModel) -> bool {
        Self::supports(self, model)
    }

    fn prepare(&mut self, model: Arc<SrModel>) -> Result<()> {
        Self::prepare(self, model)
    }

    fn encode(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        luma: &ID3D11ShaderResourceView,
        width: u32,
        height: u32,
        range: LumaRange,
    ) -> Result<ID3D11ShaderResourceView> {
        Self::encode(self, device, context, luma, width, height, range)
    }

    fn last_gpu_duration(&self) -> Option<Duration> {
        Self::last_gpu_duration(self)
    }
}

/// Moves the D3D12 backend from the setup thread to the render thread.
pub(crate) struct LinalgAccelerator(
    pub(crate) crate::renderer::d3d11_d3d12_linalg::D3d12LinalgUpscaler,
);

// SAFETY: the backend is created and prepared on the setup thread, then moved
// to the render thread and used only there; it is never shared. Its D3D11 and
// D3D12 interfaces are free-threaded, and the DXC compiler and module handle
// it holds are only touched by whichever thread currently owns the value.
unsafe impl Send for LinalgAccelerator {}

impl SrAccelerator for LinalgAccelerator {
    fn label(&self) -> &'static str {
        "linalg_matrix"
    }

    fn status(&self) -> LumaUpscalerBackendStatus {
        LumaUpscalerBackendStatus::LinalgMatrix
    }

    fn supports(&self, model: &SrModel) -> bool {
        self.0.supports(model)
    }

    fn prepare(&mut self, model: Arc<SrModel>) -> Result<()> {
        self.0.prepare(model)
    }

    fn encode(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        luma: &ID3D11ShaderResourceView,
        width: u32,
        height: u32,
        range: LumaRange,
    ) -> Result<ID3D11ShaderResourceView> {
        self.0.encode(device, context, luma, width, height, range)
    }

    fn last_gpu_duration(&self) -> Option<Duration> {
        self.0.last_gpu_duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::renderer::d3d11_sr::tests::{compare, readback_r16f, test_device, upload_luma};
    use crate::renderer::sr::catalog::source_model;
    use crate::renderer::sr::reference::{run, test_pattern};

    /// Runs the real selection (scalar + every available accelerator) until
    /// calibration finishes, checking every frame against the oracle.
    #[test]
    fn selection_calibrates_and_every_candidate_matches_the_reference() {
        let Some((device, context)) = test_device() else {
            eprintln!("skipping: no D3D11 device");
            return;
        };
        for (mode, width, height) in [
            (LumaUpscalerMode::ArtCnnC4F16, 160u32, 96u32),
            (LumaUpscalerMode::ArtCnnC4F32, 640, 360),
            (LumaUpscalerMode::ArtCnnR8F64, 96, 64),
        ] {
            let luma = test_pattern(width as usize, height as usize, 9);
            let view = upload_luma(&device, &luma, width, height);
            let expected = run(
                &source_model(mode).unwrap(),
                &luma,
                width as usize,
                height as usize,
                LumaRange::Full,
            );
            let mut upscaler = D3d11Upscaler::new(crate::renderer::d3d11::sr_accelerators);
            upscaler.attach_device(&device, mode).unwrap();
            let started = std::time::Instant::now();
            let mut frames = 0u64;
            let mut engines = std::collections::BTreeSet::new();
            loop {
                assert!(
                    started.elapsed() < Duration::from_secs(600),
                    "selection timed out"
                );
                let Some(output) = upscaler
                    .encode(
                        &device,
                        &context,
                        &view,
                        width,
                        height,
                        LumaRange::Full,
                        Some(frames),
                    )
                    .unwrap()
                else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                frames += 1;
                engines.insert(upscaler.engine());
                let actual = readback_r16f(&device, &context, &output.view, width * 2, height * 2);
                let (mae, max) = compare(&actual, &expected);
                assert!(
                    mae < 2e-3 && max < 3e-2,
                    "{} frame {frames} on {}: mae {mae} max {max}",
                    mode.label(),
                    upscaler.engine()
                );
                if upscaler.calibration.is_none() && upscaler.calibrated_for.is_some() && frames > 4
                {
                    break;
                }
                // Pace like playback so the off-thread accelerator setup can
                // finish before calibration.
                std::thread::sleep(Duration::from_millis(10));
            }
            let model = crate::renderer::sr::catalog::execution_model(mode).unwrap();
            let supported: Vec<(&str, bool)> = upscaler
                .accelerators
                .as_ref()
                .map(|accelerators| {
                    accelerators
                        .iter()
                        .map(|accelerator| (accelerator.label(), accelerator.supports(&model)))
                        .collect()
                })
                .unwrap_or_default();
            eprintln!("{}: accelerators support {supported:?}", mode.label());
            eprintln!(
                "{}: {frames} frames in {:.1}s, ran {:?}, selected {} ({:?}); unavailable: {:?}",
                mode.label(),
                started.elapsed().as_secs_f64(),
                engines,
                upscaler.engine(),
                upscaler.status(),
                upscaler.unavailable
            );
        }
    }
}
