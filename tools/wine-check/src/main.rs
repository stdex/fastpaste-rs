//! Drives fastpaste's real Windows paste path under Wine: the real
//! `fastpaste_app::Paster` over the real `SystemClipboard`,
//! `SystemPasteKeys` and `SystemForeground`, pasting into an EDIT control
//! owned by a separate GUI thread, and reports what arrived.
//!
//! Scenarios run in order; name some on the command line to run only
//! those (`bigimage` runs only when named). See README.md.
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use fastpaste_app::{PasteConfig, Paster};
use fastpaste_platform::{
    Clipboard, ClipboardSnapshot, ForegroundToken, ForegroundWindow, PasteKeys, PasteMethod,
    SystemClipboard, SystemForeground, SystemPasteKeys,
};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetFocus, SetFocus};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Like a real editor: activating the top-level puts the caret back in
/// its edit control.
static TARGET_EDIT: AtomicIsize = AtomicIsize::new(0);
/// (vk, scan, up) for every key message the target thread pulls.
static KEYS_SEEN: std::sync::Mutex<Vec<(u32, u32, bool)>> = std::sync::Mutex::new(Vec::new());
static DECOY_EDIT: AtomicIsize = AtomicIsize::new(0);

unsafe extern "system" fn plain_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if m == WM_SETFOCUS {
        let edit = TARGET_EDIT.load(Ordering::SeqCst);
        if edit != 0 {
            unsafe { SetFocus(edit as HWND) };
            return 0;
        }
    }
    unsafe { DefWindowProcW(h, m, wp, lp) }
}

unsafe extern "system" fn decoy_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if m == WM_SETFOCUS {
        let edit = DECOY_EDIT.load(Ordering::SeqCst);
        if edit != 0 {
            unsafe { SetFocus(edit as HWND) };
            return 0;
        }
    }
    unsafe { DefWindowProcW(h, m, wp, lp) }
}

/// A second top-level window with its own edit control — stands in for
/// whatever the desktop focuses after our dialog hides.
fn decoy_thread(tx: mpsc::Sender<(isize, isize)>) {
    unsafe {
        let inst = GetModuleHandleW(null());
        let cls = w("fpDecoy");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(decoy_proc);
        wc.hInstance = inst;
        wc.lpszClassName = cls.as_ptr();
        RegisterClassW(&wc);
        let title = w("decoy");
        let top = CreateWindowExW(
            0,
            cls.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            50,
            50,
            400,
            200,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        let edit_cls = w("EDIT");
        let edit = CreateWindowExW(
            0,
            edit_cls.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | (ES_MULTILINE as u32) | (ES_AUTOVSCROLL as u32),
            0,
            0,
            380,
            150,
            top,
            null_mut(),
            inst,
            null(),
        );
        DECOY_EDIT.store(edit as isize, Ordering::SeqCst);
        tx.send((top as isize, edit as isize)).unwrap();
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn target_thread(tx: mpsc::Sender<(isize, isize, bool, bool)>) {
    unsafe {
        let inst = GetModuleHandleW(null());
        let cls = w("fpTarget");
        let mut wc: WNDCLASSW = std::mem::zeroed();
        wc.lpfnWndProc = Some(plain_proc);
        wc.hInstance = inst;
        wc.lpszClassName = cls.as_ptr();
        RegisterClassW(&wc);
        let title = w("target");
        let top = CreateWindowExW(
            0,
            cls.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            0,
            0,
            400,
            200,
            null_mut(),
            null_mut(),
            inst,
            null(),
        );
        let edit_cls = w("EDIT");
        let edit = CreateWindowExW(
            0,
            edit_cls.as_ptr(),
            null(),
            WS_CHILD | WS_VISIBLE | (ES_MULTILINE as u32) | (ES_AUTOVSCROLL as u32),
            0,
            0,
            380,
            150,
            top,
            null_mut(),
            inst,
            null(),
        );
        TARGET_EDIT.store(edit as isize, Ordering::SeqCst);
        SetForegroundWindow(top);
        SetFocus(edit);
        let fg_ok = GetForegroundWindow() == top;
        let focus_ok = GetFocus() == edit;
        tx.send((top as isize, edit as isize, fg_ok, focus_ok))
            .unwrap();
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            if matches!(
                msg.message,
                WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP
            ) {
                KEYS_SEEN.lock().unwrap().push((
                    msg.wParam as u32,
                    ((msg.lParam >> 16) & 0xFF) as u32,
                    matches!(msg.message, WM_KEYUP | WM_SYSKEYUP),
                ));
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn edit_text(edit: HWND) -> String {
    let mut buf = vec![0u16; 4096];
    let n = unsafe { SendMessageW(edit, WM_GETTEXT, buf.len(), buf.as_mut_ptr() as LPARAM) };
    String::from_utf16_lossy(&buf[..n as usize])
}

fn clear_edit(edit: HWND) {
    let empty = w("");
    unsafe { SendMessageW(edit, WM_SETTEXT, 0, empty.as_ptr() as LPARAM) };
}

fn cfg(delay_ms: u64, restore_delay_ms: u64) -> PasteConfig {
    PasteConfig {
        delay_ms,
        restore_clipboard: true,
        restore_delay_ms,
        method: PasteMethod::CtrlV,
    }
}

struct Env {
    clip: Arc<SystemClipboard>,
    keys: Arc<SystemPasteKeys>,
    edit: HWND,
    changes: mpsc::Receiver<fastpaste_platform::ClipboardPayload>,
}

fn drain(env: &Env) -> Vec<String> {
    std::thread::sleep(Duration::from_millis(300));
    env.changes.try_iter().map(|p| p.text).collect()
}

fn scenario_listener(env: &Env) {
    drain(env);
    env.clip.set_text("listener-probe-1").unwrap();
    let got = env.changes.recv_timeout(Duration::from_millis(1500));
    println!(
        "[listener] WindowsClipboard history saw the copy: {}",
        match got {
            Ok(p) => format!("YES ({:?})", p.text),
            Err(_) => "NO (nothing on changes() within 1.5 s)".to_string(),
        }
    );
}

fn scenario_paste(env: &Env) {
    const TRIALS: usize = 12;
    println!(
        "[paste] real Paster, delay_ms=70, restore=true; {TRIALS} trials per restore_delay_ms"
    );
    println!(
        "[paste] restore_delay_ms | payload pasted | ORIGINAL pasted | nothing | errors | clipboard restored"
    );
    for restore_delay in [0u64, 25, 120] {
        let paster = Paster::new(env.clip.clone(), env.keys.clone(), cfg(70, restore_delay));
        let (mut ok, mut wrong, mut none, mut err, mut restored) = (0, 0, 0, 0, 0);
        for i in 0..TRIALS {
            clear_edit(env.edit);
            env.clip.set_text("ORIGINAL").unwrap();
            std::thread::sleep(Duration::from_millis(80));
            let payload = format!("PAYLOAD-{restore_delay}-{i}");
            if let Err(e) = paster.paste_text(&payload) {
                err += 1;
                eprintln!("  trial error: {e}");
                continue;
            }
            std::thread::sleep(Duration::from_millis(400));
            let t = edit_text(env.edit);
            if t.contains(&payload) {
                ok += 1
            } else if t.contains("ORIGINAL") {
                wrong += 1
            } else {
                none += 1
            }
            if env.clip.text().ok().as_deref() == Some("ORIGINAL") {
                restored += 1;
            }
        }
        println!(
            "[paste] {restore_delay:>16} | {ok:>14} | {wrong:>15} | {none:>7} | {err:>6} | {restored:>2}/{TRIALS}"
        );
    }
    let seen = drain(env);
    let leaked = seen.iter().filter(|t| t.starts_with("PAYLOAD-")).count();
    println!(
        "[paste] history events: {} total, {leaked} of them the paster's own payloads (want 0)",
        seen.len()
    );
}

/// A second clipboard handle copies "USER-NEW" while the Paster is waiting
/// to restore; the user's new copy must survive.
fn scenario_midcopy(env: &Env) {
    const TRIALS: usize = 6;
    let other = Arc::new(SystemClipboard::new().expect("second clipboard"));
    let paster = Paster::new(env.clip.clone(), env.keys.clone(), cfg(70, 400));
    let mut kept = 0;
    for i in 0..TRIALS {
        clear_edit(env.edit);
        env.clip.set_text("ORIGINAL").unwrap();
        std::thread::sleep(Duration::from_millis(80));
        let other = Arc::clone(&other);
        let copier = std::thread::spawn(move || {
            // 70 ms settle + ~200 ms into the 400 ms restore wait.
            std::thread::sleep(Duration::from_millis(270));
            other.set_text("USER-NEW").unwrap();
        });
        let _ = paster.paste_text(&format!("MID-{i}"));
        copier.join().unwrap();
        let now = env.clip.text().unwrap_or_default();
        if now == "USER-NEW" {
            kept += 1;
        } else {
            eprintln!("  trial {i}: clipboard after paste = {now:?}");
        }
    }
    println!("[midcopy] user's mid-paste copy survived: {kept}/{TRIALS}");
}

/// The hotkey captures the target; a decoy then takes the foreground (as
/// the dialog and the desktop's refocus would). With the captured token
/// the paste must land in the target; with none it lands in the decoy.
fn scenario_focus(env: &Env) {
    const TRIALS: usize = 6;
    let fg = Arc::new(SystemForeground::new().expect("foreground"));
    let token = fg.capture();
    println!("[focus] captured target token: {:?}", token.raw().is_some());

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || decoy_thread(tx));
    let (decoy_top, decoy_edit) = rx.recv().unwrap();
    let decoy_top = decoy_top as HWND;
    let decoy_edit = decoy_edit as HWND;

    let paster =
        Paster::new(env.clip.clone(), env.keys.clone(), cfg(70, 120)).with_foreground(fg.clone());
    // Same window id, different owner: what a recycled HWND looks like.
    let recycled = ForegroundToken::with_owner(token.raw().unwrap(), 1);
    for (label, target) in [
        ("with captured target", token),
        ("without a target", ForegroundToken::default()),
        ("recycled id (wrong owner)", recycled),
    ] {
        let (mut in_target, mut in_decoy, mut nowhere) = (0, 0, 0);
        for i in 0..TRIALS {
            clear_edit(env.edit);
            clear_edit(decoy_edit);
            unsafe { SetForegroundWindow(decoy_top) };
            std::thread::sleep(Duration::from_millis(100));
            let payload = format!("FOCUS-{i}");
            paster.paste_text_to(&payload, &target).unwrap();
            std::thread::sleep(Duration::from_millis(300));
            if edit_text(env.edit).contains(&payload) {
                in_target += 1
            } else if edit_text(decoy_edit).contains(&payload) {
                in_decoy += 1
            } else {
                nowhere += 1
            }
        }
        println!(
            "[focus] {label}: target {in_target}/{TRIALS}, decoy {in_decoy}/{TRIALS}, nowhere {nowhere}"
        );
    }
    unsafe { SetForegroundWindow(GetParent(env.edit)) };
}

/// Which scan codes reach the target. Control first: a raw SendInput with
/// wScan = 0 (what the old code sent) must show up as scan 0, proving the
/// probe can tell the difference.
fn scenario_scan(env: &Env) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    let raw = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    KEYS_SEEN.lock().unwrap().clear();
    let inputs = [
        raw(VK_CONTROL, false),
        raw(VK_V, false),
        raw(VK_V, true),
        raw(VK_CONTROL, true),
    ];
    unsafe { SendInput(4, inputs.as_ptr(), size_of::<INPUT>() as i32) };
    std::thread::sleep(Duration::from_millis(300));
    println!("[scan] control (wScan=0): {:x?}", KEYS_SEEN.lock().unwrap());

    KEYS_SEEN.lock().unwrap().clear();
    let paster = Paster::new(env.clip.clone(), env.keys.clone(), cfg(70, 120));
    paster.paste_text("SCAN").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let seen = KEYS_SEEN.lock().unwrap().clone();
    println!("[scan] real Paster: {seen:x?}");
    let zero = seen.iter().filter(|(_, scan, _)| *scan == 0).count();
    println!("[scan] key messages with scan 0: {zero} of {}", seen.len());
}

/// The user is still holding Alt+Shift when the chord fires. Control: a
/// raw Ctrl+V sent with them held (what the old builder did) must not
/// paste; the real Paster releases them first and must.
fn scenario_held(env: &Env) {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
    let raw = |vk: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_VSC) } as u16,
                dwFlags: if up { KEYEVENTF_KEYUP } else { 0 },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let send = |inputs: &[INPUT]| unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<INPUT>() as i32,
        )
    };
    let hold = || {
        send(&[raw(VK_LMENU, false), raw(VK_LSHIFT, false)]);
        std::thread::sleep(Duration::from_millis(100));
    };
    let let_go = || {
        send(&[raw(VK_LSHIFT, true), raw(VK_LMENU, true)]);
        std::thread::sleep(Duration::from_millis(100));
    };
    let held_now = || unsafe {
        (
            GetAsyncKeyState(VK_LMENU as i32) < 0,
            GetAsyncKeyState(VK_LSHIFT as i32) < 0,
        )
    };

    // Control.
    clear_edit(env.edit);
    env.clip.set_text("CONTROL").unwrap();
    hold();
    println!(
        "[held] async state while held (alt, shift): {:?}",
        held_now()
    );
    send(&[
        raw(VK_CONTROL, false),
        raw(VK_V, false),
        raw(VK_V, true),
        raw(VK_CONTROL, true),
    ]);
    std::thread::sleep(Duration::from_millis(300));
    let_go();
    println!(
        "[held] control (chord sent with Alt+Shift held) pasted: {}",
        edit_text(env.edit).contains("CONTROL")
    );

    // Real Paster.
    let paster = Paster::new(env.clip.clone(), env.keys.clone(), cfg(70, 120));
    let mut ok = 0;
    const TRIALS: usize = 6;
    for i in 0..TRIALS {
        clear_edit(env.edit);
        hold();
        KEYS_SEEN.lock().unwrap().clear();
        let payload = format!("HELD-{i}");
        paster.paste_text(&payload).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        if edit_text(env.edit).contains(&payload) {
            ok += 1
        }
        if i == 0 {
            println!("[held] key messages: {:x?}", KEYS_SEEN.lock().unwrap());
            println!(
                "[held] async state after paste (alt, shift): {:?}",
                held_now()
            );
        }
        let_go();
    }
    println!("[held] real Paster with Alt+Shift held pasted: {ok}/{TRIALS}");
}

/// The user's clipboard holds an image, a file list, or a format nobody
/// here can represent. Each must survive a paste (or the paste cancels).
fn scenario_nontext(env: &Env) {
    use windows_sys::Win32::System::DataExchange::*;
    use windows_sys::Win32::System::Memory::*;
    let paster = Paster::new(env.clip.clone(), env.keys.clone(), cfg(70, 120));

    // An image as an application leaves it, read back through the
    // backend so the comparison is on what a snapshot holds.
    let mut ab = arboard::Clipboard::new().unwrap();
    ab.set_image(arboard::ImageData {
        width: 2,
        height: 2,
        bytes: vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ]
        .into(),
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let image = env.clip.snapshot();
    assert!(matches!(image, ClipboardSnapshot::Image(_)), "{image:?}");
    let files = ClipboardSnapshot::Files(vec![r"C:\windows\win.ini".into()]);
    for (label, original) in [("image", image), ("files", files)] {
        clear_edit(env.edit);
        env.clip.restore(&original).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        println!(
            "[nontext] {label}: what the old text() snapshot saw: {:?}",
            env.clip.text().map(|_| "text")
        );
        let res = paster.paste_text(&format!("NT-{label}"));
        std::thread::sleep(Duration::from_millis(300));
        let pasted = edit_text(env.edit).contains(&format!("NT-{label}"));
        let after = env.clip.snapshot();
        println!(
            "[nontext] {label}: paste {res:?}, pasted={pasted}, clipboard restored intact={}",
            after == original
        );
        if after != original {
            println!("[nontext]   after = {after:?}");
        }
    }

    // A private format: nothing we can save.
    let name: Vec<u16> = "FASTPASTE_TEST_FORMAT\0".encode_utf16().collect();
    let fmt = unsafe { RegisterClipboardFormatW(name.as_ptr()) };
    unsafe {
        let bytes = b"opaque";
        let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len());
        let p = GlobalLock(h) as *mut u8;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
        GlobalUnlock(h);
        assert!(OpenClipboard(null_mut()) != 0);
        EmptyClipboard();
        SetClipboardData(fmt, h as _);
        CloseClipboard();
    }
    clear_edit(env.edit);
    let res = paster.paste_text("NT-private");
    std::thread::sleep(Duration::from_millis(300));
    let still_there = unsafe { IsClipboardFormatAvailable(fmt) } != 0;
    println!(
        "[nontext] private format: paste {res:?}, pasted={}, format still on clipboard={still_there}",
        edit_text(env.edit).contains("NT-private")
    );
}

/// The Shift+Insert method, through the real Paster.
fn scenario_shiftins(env: &Env) {
    const TRIALS: usize = 6;
    let mut c = cfg(70, 120);
    c.method = PasteMethod::ShiftInsert;
    let paster = Paster::new(env.clip.clone(), env.keys.clone(), c);
    let mut ok = 0;
    for i in 0..TRIALS {
        clear_edit(env.edit);
        env.clip.set_text("ORIGINAL").unwrap();
        KEYS_SEEN.lock().unwrap().clear();
        let payload = format!("SHINS-{i}");
        paster.paste_text(&payload).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        if edit_text(env.edit).contains(&payload) {
            ok += 1
        }
        if i == 0 {
            println!("[shiftins] key messages: {:x?}", KEYS_SEEN.lock().unwrap());
        }
    }
    println!(
        "[shiftins] Shift+Insert pasted: {ok}/{TRIALS}, clipboard after: {:?}",
        env.clip.text().ok()
    );
}

/// A 4000x3000 image (a large screenshot): how long do snapshot and
/// restore take, and does the image come back pixel-identical?
fn scenario_bigimage(env: &Env) {
    let (w, h) = (4000usize, 3000usize);
    let mut rgba = vec![0u8; w * h * 4];
    for (i, px) in rgba.chunks_mut(4).enumerate() {
        px.copy_from_slice(&[
            (i % 251) as u8,
            (i / w % 241) as u8,
            (i % 7 * 30) as u8,
            255,
        ]);
    }
    let mut ab = arboard::Clipboard::new().unwrap();
    ab.set_image(arboard::ImageData {
        width: w,
        height: h,
        bytes: rgba.clone().into(),
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(200));

    let t = std::time::Instant::now();
    let snap = env.clip.snapshot();
    let snap_ms = t.elapsed().as_millis();
    env.clip.set_text("payload over the image").unwrap();
    let t = std::time::Instant::now();
    let res = env.clip.restore(&snap);
    let restore_ms = t.elapsed().as_millis();
    std::thread::sleep(Duration::from_millis(200));
    let back = ab
        .get_image()
        .map(|i| (i.width, i.height, i.bytes.into_owned() == rgba));
    println!(
        "[bigimage] snapshot {snap_ms} ms, restore {restore_ms} ms ({res:?}); read back (w, h, identical): {back:?}"
    );
}

fn main() {
    let which: Vec<String> = std::env::args().skip(1).collect();
    let run = |name: &str| which.is_empty() || which.iter().any(|w| w == name);

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || target_thread(tx));
    let (_top, edit, fg_ok, focus_ok) = rx.recv().unwrap();
    println!("target: foreground={fg_ok} focus_on_edit={focus_ok}");

    let clip = Arc::new(SystemClipboard::new().expect("clipboard"));
    let changes = clip.changes();
    let env = Env {
        changes,
        clip,
        keys: Arc::new(SystemPasteKeys::new().expect("keys")),
        edit: edit as HWND,
    };
    assert!(env.keys.available());

    if run("listener") {
        scenario_listener(&env);
    }
    if run("paste") {
        scenario_paste(&env);
    }
    if run("focus") {
        scenario_focus(&env);
    }
    if run("scan") {
        scenario_scan(&env);
    }
    if run("held") {
        scenario_held(&env);
    }
    if run("nontext") {
        scenario_nontext(&env);
    }
    if run("shiftins") {
        scenario_shiftins(&env);
    }
    // Opt-in only: slow by design.
    if which.iter().any(|w| w == "bigimage") {
        scenario_bigimage(&env);
    }
    // Last: it creates a second in-process SystemClipboard, and under Wine
    // two instances opening the clipboard with a null window can close each
    // other's sessions — something the app, with its single instance, never
    // does.
    if run("midcopy") {
        scenario_midcopy(&env);
    }
}
