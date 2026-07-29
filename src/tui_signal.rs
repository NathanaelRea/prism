use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct SigtermNotification {
    requested: Arc<AtomicBool>,
    #[cfg(unix)]
    registration: Option<signal_hook::SigId>,
}

impl SigtermNotification {
    pub(crate) fn install() -> Result<Self, String> {
        let requested = Arc::new(AtomicBool::new(false));
        #[cfg(unix)]
        let registration =
            signal_hook::flag::register(signal_hook::consts::SIGTERM, requested.clone())
                .map_err(|error| format!("install SIGTERM notification: {error}"))?;
        Ok(Self {
            requested,
            #[cfg(unix)]
            registration: Some(registration),
        })
    }

    pub(crate) fn requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            requested: Arc::new(AtomicBool::new(false)),
            #[cfg(unix)]
            registration: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn request_for_test(&self) {
        self.requested.store(true, Ordering::Release);
    }
}

#[cfg(unix)]
impl Drop for SigtermNotification {
    fn drop(&mut self) {
        if let Some(registration) = self.registration.take() {
            signal_hook::low_level::unregister(registration);
        }
    }
}
