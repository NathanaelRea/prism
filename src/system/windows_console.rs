use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;

use windows::core::BOOL;
use windows::Win32::System::Console::{
    SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
    CTRL_SHUTDOWN_EVENT,
};

const CTRL_C: u8 = 1;
const TERMINATE: u8 = 2;

struct ConsoleControlState {
    canceled: Arc<AtomicBool>,
    signal: Arc<AtomicU8>,
}

static STATE: OnceLock<ConsoleControlState> = OnceLock::new();
static REGISTERED: OnceLock<Result<(), String>> = OnceLock::new();
static ATTACHED_CHILD_OWNS_INTERRUPT: AtomicBool = AtomicBool::new(false);

extern "system" fn handler(control: u32) -> BOOL {
    let Some(state) = STATE.get() else {
        return false.into();
    };
    let code = match control {
        CTRL_C_EVENT | CTRL_BREAK_EVENT => {
            if ATTACHED_CHILD_OWNS_INTERRUPT.load(Ordering::Acquire) {
                // The attached psmux client shares this console process group and receives the
                // same event. Keep Prism alive without turning the child's interrupt into a TUI
                // shutdown request.
                return true.into();
            }
            CTRL_C
        }
        CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT | CTRL_SHUTDOWN_EVENT => TERMINATE,
        _ => return false.into(),
    };
    let _ = state
        .signal
        .compare_exchange(0, code, Ordering::AcqRel, Ordering::Acquire);
    state.canceled.store(true, Ordering::Release);
    true.into()
}

fn install() -> io::Result<&'static ConsoleControlState> {
    let state = STATE.get_or_init(|| ConsoleControlState {
        canceled: Arc::new(AtomicBool::new(false)),
        signal: Arc::new(AtomicU8::new(0)),
    });
    let registered = REGISTERED.get_or_init(|| {
        // SAFETY: handler has process lifetime and only touches lock-free static state.
        unsafe { SetConsoleCtrlHandler(Some(handler), true) }.map_err(|error| error.to_string())
    });
    match registered {
        Ok(()) => Ok(state),
        Err(error) => Err(io::Error::other(error.clone())),
    }
}

pub(crate) fn cancellation() -> io::Result<Arc<AtomicBool>> {
    install().map(|state| Arc::clone(&state.canceled))
}

pub(crate) fn signal_code() -> io::Result<Arc<AtomicU8>> {
    install().map(|state| Arc::clone(&state.signal))
}

pub(crate) const fn is_interrupt(code: u8) -> bool {
    code == CTRL_C
}

pub(crate) struct AttachedChildInterruptGuard;

pub(crate) fn attached_child_owns_interrupt() -> io::Result<AttachedChildInterruptGuard> {
    install()?;
    ATTACHED_CHILD_OWNS_INTERRUPT
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| io::Error::other("an attached console child already owns Ctrl+C"))?;
    Ok(AttachedChildInterruptGuard)
}

impl Drop for AttachedChildInterruptGuard {
    fn drop(&mut self) {
        ATTACHED_CHILD_OWNS_INTERRUPT.store(false, Ordering::Release);
    }
}
