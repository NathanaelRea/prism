use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShutdownSignal {
    Sigint,
    Sigterm,
}

impl ShutdownSignal {
    #[cfg(any(unix, test))]
    const fn code(self) -> u8 {
        match self {
            Self::Sigint => 1,
            Self::Sigterm => 2,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Sigint),
            2 => Some(Self::Sigterm),
            _ => None,
        }
    }
}

pub(crate) struct ShutdownNotification {
    #[allow(dead_code)]
    canceled: Arc<AtomicBool>,
    signal: Arc<AtomicU8>,
    #[cfg(unix)]
    registrations: Vec<signal_hook::SigId>,
}

impl ShutdownNotification {
    pub(crate) fn install() -> Result<Self, String> {
        #[cfg(unix)]
        let canceled = Arc::new(AtomicBool::new(false));
        #[cfg(unix)]
        let signal = Arc::new(AtomicU8::new(0));
        #[cfg(windows)]
        let canceled = crate::system::windows_console::cancellation()
            .map_err(|error| format!("install shutdown notification: {error}"))?;
        #[cfg(windows)]
        let signal = crate::system::windows_console::signal_code()
            .map_err(|error| format!("install shutdown notification: {error}"))?;
        #[cfg(unix)]
        let registrations = {
            let mut registrations = Vec::new();
            for (raw_signal, shutdown_signal) in [
                (signal_hook::consts::SIGINT, ShutdownSignal::Sigint),
                (signal_hook::consts::SIGTERM, ShutdownSignal::Sigterm),
            ] {
                let canceled = Arc::clone(&canceled);
                let signal = Arc::clone(&signal);
                let registration = unsafe {
                    signal_hook::low_level::register(raw_signal, move || {
                        let _ = signal.compare_exchange(
                            0,
                            shutdown_signal.code(),
                            Ordering::AcqRel,
                            Ordering::Acquire,
                        );
                        canceled.store(true, Ordering::Release);
                    })
                };
                match registration {
                    Ok(registration) => registrations.push(registration),
                    Err(error) => {
                        for registration in registrations {
                            signal_hook::low_level::unregister(registration);
                        }
                        return Err(format!("install shutdown notification: {error}"));
                    }
                }
            }
            registrations
        };
        Ok(Self {
            canceled,
            signal,
            #[cfg(unix)]
            registrations,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn cancellation(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.canceled)
    }

    pub(crate) fn signal(&self) -> Option<ShutdownSignal> {
        let code = self.signal.load(Ordering::Acquire);
        #[cfg(windows)]
        if crate::system::windows_console::is_interrupt(code) {
            return Some(ShutdownSignal::Sigint);
        }
        ShutdownSignal::from_code(code)
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            canceled: Arc::new(AtomicBool::new(false)),
            signal: Arc::new(AtomicU8::new(0)),
            #[cfg(unix)]
            registrations: Vec::new(),
        }
    }

    #[cfg(test)]
    pub(crate) fn request_for_test(&self, shutdown_signal: ShutdownSignal) {
        let _ = self.signal.compare_exchange(
            0,
            shutdown_signal.code(),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.canceled.store(true, Ordering::Release);
    }
}

#[cfg(unix)]
impl Drop for ShutdownNotification {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ShutdownNotification, ShutdownSignal};
    use std::sync::atomic::Ordering;

    #[test]
    fn shutdown_notification_records_reason_before_cancellation() {
        let notification = ShutdownNotification::for_test();

        notification.request_for_test(ShutdownSignal::Sigint);

        assert_eq!(notification.signal(), Some(ShutdownSignal::Sigint));
        assert!(notification.cancellation().load(Ordering::Acquire));
    }

    #[test]
    fn shutdown_notification_keeps_the_first_signal() {
        let notification = ShutdownNotification::for_test();

        notification.request_for_test(ShutdownSignal::Sigterm);
        notification.request_for_test(ShutdownSignal::Sigint);

        assert_eq!(notification.signal(), Some(ShutdownSignal::Sigterm));
    }
}
