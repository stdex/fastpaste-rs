//! Windows backend: arboard for reads and writes, and a clipboard format
//! listener for change notifications.
//!
//! Selected as `SystemClipboard` on Windows.
//!
//! Change detection is event-driven and needs no polling at all:
//! `AddClipboardFormatListener` asks the OS to post `WM_CLIPBOARDUPDATE`
//! to a window whenever the clipboard's contents change. A
//! message-only window (`HWND_MESSAGE`) is enough — it is never shown,
//! never gets focus, and exists purely to own a message queue.
//!
//! Own writes: `set_text` from this process raises the same
//! notification as any other application's copy, so the paste sequence
//! announces its writes through [`Clipboard::suppress_text`] and they
//! are recorded but not reported to the history consumer.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::AddClipboardFormatListener;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetMessageW, HWND_MESSAGE, MSG, RegisterClassW,
    WM_CLIPBOARDUPDATE, WNDCLASSW,
};

use super::{Clipboard, ClipboardError, ClipboardPayload, SuppressList};
use crate::take_once::TakeOnceChannel;

/// Clipboard read/write plus an event-driven change stream.
///
/// `arboard::Clipboard` is not `Sync`, so it sits behind a `Mutex` and
/// is shared with the listener thread — which needs it to read the new
/// contents once the OS says they changed.
pub struct WindowsClipboard {
    inner: Arc<Mutex<arboard::Clipboard>>,
    changes: TakeOnceChannel<ClipboardPayload>,
    suppress: Arc<SuppressList>,
}

impl WindowsClipboard {
    pub fn new() -> Result<Self, ClipboardError> {
        let clip = arboard::Clipboard::new().map_err(ClipboardError::from_arboard)?;
        let inner = Arc::new(Mutex::new(clip));
        let suppress = Arc::new(SuppressList::default());
        let changes = TakeOnceChannel::new();

        let thread_inner = Arc::clone(&inner);
        let thread_suppress = Arc::clone(&suppress);
        let thread_tx = changes.sender();
        thread::Builder::new()
            .name("clipboard-listener".into())
            .spawn(move || listener_loop(thread_inner, thread_tx, thread_suppress))
            .map_err(|e| ClipboardError::Access(Box::new(e)))?;

        Ok(Self {
            inner,
            changes,
            suppress,
        })
    }
}

impl Clipboard for WindowsClipboard {
    fn set_text(&self, text: &str) -> Result<(), ClipboardError> {
        // Recover from poisoning rather than panic: this runs on the UI
        // thread, and a panic inside a Slint callback unwinds through
        // the event loop and takes the process down.
        let mut clip = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        clip.set_text(text.to_owned())
            .map_err(ClipboardError::from_arboard)
    }

    fn text(&self) -> Result<String, ClipboardError> {
        let mut clip = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        clip.get_text().map_err(ClipboardError::from_arboard)
    }

    fn changes(&self) -> Receiver<ClipboardPayload> {
        self.changes.take()
    }

    fn suppress_text(&self, text: &str) {
        self.suppress.arm(text);
    }
}

/// State the window procedure needs, handed over through the window's
/// user data. Boxed and leaked for the lifetime of the thread — the
/// window outlives every message it will ever receive, and the thread
/// only ends with the process.
struct ListenerState {
    inner: Arc<Mutex<arboard::Clipboard>>,
    tx: std::sync::mpsc::Sender<ClipboardPayload>,
    suppress: Arc<SuppressList>,
    last_seen: Mutex<Option<String>>,
}

impl ListenerState {
    /// Read the clipboard and report it, unless it is one of our own
    /// writes or a repeat of what we already reported.
    fn on_change(&self) {
        let text = {
            let mut clip = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match clip.get_text() {
                Ok(t) => t,
                // Not text — an image or a format we do not handle. Not
                // an error worth reporting on every copy.
                Err(e) => {
                    tracing::trace!("clipboard-listener: get_text: {e}");
                    return;
                }
            }
        };
        if text.is_empty() {
            return;
        }

        // Own write from the paste sequence: record it so the diff below
        // stays correct, but do not report it as a user copy.
        if self.suppress.take(&text) {
            *self.last_seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
            return;
        }

        let mut last = self.last_seen.lock().unwrap_or_else(|e| e.into_inner());
        if last.as_deref() == Some(text.as_str()) {
            return;
        }
        *last = Some(text.clone());
        drop(last);

        // Fails only if the consumer is gone.
        let _ = self.tx.send(ClipboardPayload {
            text,
            source_process: String::new(),
        });
    }
}

/// Window procedure for the message-only window.
///
/// # Safety
///
/// Called by the OS with a valid `hwnd` for the window this module
/// created. `GWLP_USERDATA` holds the `ListenerState` pointer set right
/// after creation; it is only read after that store, and the pointee
/// outlives the window.
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GWLP_USERDATA, GetWindowLongPtrW};

    if msg == WM_CLIPBOARDUPDATE {
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const ListenerState;
        if !ptr.is_null() {
            unsafe { (*ptr).on_change() };
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Create the message-only window, subscribe to clipboard updates, and
/// pump messages until the process ends.
fn listener_loop(
    inner: Arc<Mutex<arboard::Clipboard>>,
    tx: std::sync::mpsc::Sender<ClipboardPayload>,
    suppress: Arc<SuppressList>,
) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GWLP_USERDATA, SetWindowLongPtrW};

    // Seed `last_seen` with whatever is already on the clipboard, so the
    // first notification after start-up does not record the user's
    // pre-existing content as a fresh copy.
    let seed = {
        let mut clip = inner.lock().unwrap_or_else(|e| e.into_inner());
        clip.get_text().ok()
    };

    let state = Box::new(ListenerState {
        inner,
        tx,
        suppress,
        last_seen: Mutex::new(seed),
    });
    let state = Box::into_raw(state);

    let class_name: Vec<u16> = "fastpaste_clipboard_listener\0".encode_utf16().collect();

    // SAFETY: `GetModuleHandleW(null)` returns this process's own
    // module handle and cannot fail here.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };

    let mut class: WNDCLASSW = unsafe { std::mem::zeroed() };
    class.lpfnWndProc = Some(wnd_proc);
    class.hInstance = instance;
    class.lpszClassName = class_name.as_ptr();

    // SAFETY: `class` is fully initialised and its name pointer outlives
    // the call. A duplicate-class error is harmless — the class already
    // exists and CreateWindowExW below will find it.
    unsafe { RegisterClassW(&class) };

    // SAFETY: creating a message-only window; every pointer argument is
    // either null or the class name that outlives this call.
    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        tracing::error!("clipboard-listener: could not create the message window");
        // Nothing will ever read the state now.
        drop(unsafe { Box::from_raw(state) });
        return;
    }

    // SAFETY: `hwnd` is valid and `state` outlives the window (it is
    // only freed after DestroyWindow below).
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize) };

    // SAFETY: `hwnd` is a window this thread owns.
    if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
        tracing::error!("clipboard-listener: AddClipboardFormatListener failed");
        unsafe { DestroyWindow(hwnd) };
        drop(unsafe { Box::from_raw(state) });
        return;
    }

    tracing::debug!("clipboard-listener: started");

    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: `msg` is a live, initialised MSG owned by this frame; the
    // null window filter means "any window of this thread".
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        // Nothing to translate or dispatch beyond the window procedure,
        // which the OS calls directly for WM_CLIPBOARDUPDATE.
        unsafe { DefWindowProcW(msg.hwnd, msg.message, msg.wParam, msg.lParam) };
    }

    // SAFETY: tearing down what this function created, in order.
    unsafe {
        use windows_sys::Win32::System::DataExchange::RemoveClipboardFormatListener;
        RemoveClipboardFormatListener(hwnd);
        DestroyWindow(hwnd);
    }
    drop(unsafe { Box::from_raw(state) });
    tracing::info!("clipboard-listener: exiting");
}
