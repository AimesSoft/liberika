//! Cancellation boundary for persistent HTTP read-ahead streams.
//!
//! Foreground and persistent background reads use reqwest on one shared Tokio
//! runtime. Cancellation drops the request future and interrupts connect, TLS,
//! response headers, and response body waits. System DNS resolution already
//! running in a blocking resolver may finish separately.

use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

use super::{Result, SourceError};

/// One open's optional retry capability. It never cancels healthy network I/O.
/// Commit and cancellation are serialized; sealed handles cannot affect playback.
#[derive(Clone, Debug, Default)]
pub struct ColdOpenCancellation(Arc<Mutex<ColdOpenState>>);

/// An already reserved open capability. This deliberately has no Clone
/// implementation and cannot be constructed outside the cancellation factory.
#[derive(Debug)]
pub struct ClaimedColdOpenCancellation(ColdOpenCancellation);

impl ClaimedColdOpenCancellation {
    pub(crate) fn into_inner(self) -> ColdOpenCancellation {
        self.0
    }
}

#[derive(Debug, Default)]
struct ColdOpenState {
    used: bool,
    cancelled: bool,
    sealed: bool,
    claimed: bool,
    metadata_backoff: bool,
    source: Option<SourceCancellation>,
}

impl ColdOpenCancellation {
    /// Reserve this one-shot capability before touching a presenter. Two opens
    /// must never bind the same capability to different sources.
    pub fn claim(&self) -> Option<ClaimedColdOpenCancellation> {
        let mut state = self.0.lock().unwrap();
        if state.used || state.sealed {
            return None;
        }
        state.used = true;
        Some(ClaimedColdOpenCancellation(self.clone()))
    }

    pub fn cancel(&self) {
        let mut state = self.0.lock().unwrap();
        if !state.sealed {
            state.cancelled = true;
            if state.claimed || state.metadata_backoff {
                state.claimed = true;
                if let Some(source) = &state.source {
                    source.cancel();
                }
            }
        }
    }

    /// Seal success or failure before publication. Later cancellation is harmless.
    pub fn finish(&self) -> bool {
        let mut state = self.0.lock().unwrap();
        state.sealed = true;
        state.source = None;
        !state.cancelled || !state.claimed
    }

    pub fn is_cancelled(&self) -> bool {
        let state = self.0.lock().unwrap();
        state.cancelled && state.claimed
    }

    pub(super) fn bind_source(&self, source: SourceCancellation) {
        self.0.lock().unwrap().source = Some(source);
    }

    /// Expose only the existing metadata sleep to cancellation, without
    /// consuming the later media GET's one-shot retry capability.
    pub(super) async fn metadata_backoff(&self, duration: Duration) -> Result<()> {
        struct Window<'a> {
            state: &'a Mutex<ColdOpenState>,
        }
        impl Drop for Window<'_> {
            fn drop(&mut self) {
                self.state.lock().unwrap().metadata_backoff = false;
            }
        }
        let window = {
            let mut state = self.0.lock().unwrap();
            if state.sealed || state.claimed {
                None
            } else if state.cancelled {
                state.claimed = true;
                if let Some(source) = &state.source {
                    source.cancel();
                }
                return Err(SourceError::Cancelled);
            } else {
                state.metadata_backoff = true;
                Some(Window { state: &self.0 })
            }
        };
        tokio::time::sleep(duration).await;
        drop(window);
        Ok(())
    }

    pub(super) async fn wait(&self, duration: Duration) -> Result<()> {
        {
            let mut state = self.0.lock().unwrap();
            if state.sealed || state.claimed {
                return Err(SourceError::Http("cold-open retry unavailable".into()));
            }
            state.claimed = true;
            if state.cancelled {
                if let Some(source) = &state.source {
                    source.cancel();
                }
                return Err(SourceError::Cancelled);
            }
        }
        tokio::time::sleep(duration).await;
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct SourceCancellation(Arc<Mutex<CancellationState>>);

#[derive(Debug)]
struct CancellationState {
    token: CancellationToken,
    closed: bool,
    generation: u64,
}

impl Default for SourceCancellation {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(CancellationState {
            token: CancellationToken::new(),
            closed: false,
            generation: 0,
        })))
    }
}

impl SourceCancellation {
    pub fn cancel(&self) {
        let mut state = self.0.lock().expect("source cancellation mutex poisoned");
        state.closed = true;
        state.token.cancel();
    }

    pub(crate) fn interrupt(&self) -> u64 {
        let mut state = self.0.lock().expect("source cancellation mutex poisoned");
        state.token.cancel();
        state.generation = state.generation.wrapping_add(1);
        state.generation
    }

    pub(crate) fn resume(&self, generation: u64) {
        let mut state = self.0.lock().expect("source cancellation mutex poisoned");
        if !state.closed && state.generation == generation && state.token.is_cancelled() {
            state.token = CancellationToken::new();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.token().is_cancelled()
    }

    fn child(&self) -> Self {
        Self(Arc::new(Mutex::new(CancellationState {
            token: self.token().child_token(),
            closed: false,
            generation: 0,
        })))
    }

    fn token(&self) -> CancellationToken {
        self.0
            .lock()
            .expect("source cancellation mutex poisoned")
            .token
            .clone()
    }
}

#[derive(Clone)]
pub(super) struct HttpIo {
    pub(super) client: reqwest::Client,
    pub(super) cancellation: SourceCancellation,
    pub(super) cold_open: Option<ColdOpenCancellation>,
}

impl HttpIo {
    pub(super) fn new() -> Self {
        Self {
            client: shared_client().clone(),
            cancellation: SourceCancellation::default(),
            cold_open: None,
        }
    }

    pub(super) fn child(&self) -> Self {
        Self {
            client: self.client.clone(),
            cancellation: self.cancellation.child(),
            cold_open: None,
        }
    }

    pub(super) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub(super) fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub(super) fn run<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let cancellation = self.cancellation.token();
        let (sender, receiver) = crossbeam_channel::bounded(1);
        runtime().spawn(async move {
            // Keep the future in a narrower scope so its response, socket, and
            // partial buffers are dropped before completion is published to
            // the synchronous caller.
            let result = {
                tokio::pin!(future);
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => Err(SourceError::Cancelled),
                    result = &mut future => result,
                }
            };
            let _ = sender.send(result);
        });
        receiver
            .recv()
            .unwrap_or_else(|_| Err(SourceError::Http("HTTP worker stopped".to_string())))
    }

    pub(super) fn wait_cancelled(&self, duration: Duration) -> bool {
        self.run(async move {
            tokio::time::sleep(duration).await;
            Ok(())
        })
        .is_err()
    }
}

fn shared_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("build HTTP client")
    })
}

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("erika-http-io")
            .enable_all()
            .build()
            .expect("create HTTP I/O runtime")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_interrupt_cannot_resume_newer_or_terminal_cancellation() {
        let cancellation = SourceCancellation::default();
        let first = cancellation.interrupt();
        let latest = cancellation.interrupt();
        cancellation.resume(first);
        assert!(cancellation.is_cancelled());
        cancellation.resume(latest);
        assert!(!cancellation.is_cancelled());
        cancellation.cancel();
        cancellation.resume(latest);
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn cancellation_drops_operation_resources_before_returning() {
        struct Resource(std::sync::mpsc::Sender<()>);
        impl Drop for Resource {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        let io = HttpIo::new();
        let cancellation = io.cancellation.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            io.run(async move {
                let _resource = Resource(dropped_tx);
                started_tx.send(()).unwrap();
                std::future::pending::<Result<()>>().await
            })
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        cancellation.cancel();
        assert!(matches!(
            worker.join().unwrap(),
            Err(SourceError::Cancelled)
        ));
        assert!(dropped_rx.try_recv().is_ok());
    }
}
