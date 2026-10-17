//! Drives fastpaste's real Linux `SystemClipboard` snapshot/restore
//! against a headless KWin. `WAYLAND_DISPLAY` must name that compositor's
//! socket (starting `fp-`), so it never touches the session you work in.
//! See README.md.
use std::time::{Duration, Instant};

use fastpaste_platform::{Clipboard, ClipboardSnapshot, SystemClipboard};

fn main() {
    let display = std::env::var("WAYLAND_DISPLAY").unwrap_or_default();
    assert!(
        display.starts_with("fp-"),
        "refusing to run against {display:?}"
    );
    let clip = SystemClipboard::new().expect("clipboard");
    let mut ab = arboard::Clipboard::new().unwrap();

    // Empty clipboard.
    let _ = ab.clear();
    std::thread::sleep(Duration::from_millis(200));
    let t = Instant::now();
    let snap = clip.snapshot();
    println!(
        "[empty] snapshot {:?} in {} ms",
        snap,
        t.elapsed().as_millis()
    );

    // Plain text.
    ab.set_text("привет, clipboard").unwrap();
    std::thread::sleep(Duration::from_millis(200));
    println!("[text] snapshot {:?}", clip.snapshot());

    // Files (a file manager offers the paths as text too).
    let files = vec![std::path::PathBuf::from("/etc/hostname")];
    ab.set().file_list(&files).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    println!("[files] snapshot {:?}", clip.snapshot());

    // A large image.
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
    ab.set_image(arboard::ImageData {
        width: w,
        height: h,
        bytes: rgba.clone().into(),
    })
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    let snap = clip.snapshot();
    let snap_ms = t.elapsed().as_millis();
    let kind = match &snap {
        ClipboardSnapshot::Image(_) => "Image".to_string(),
        other => format!("{other:?}"),
    };
    clip.set_text("payload over the image").unwrap();
    let t = Instant::now();
    let res = clip.restore(&snap);
    let restore_ms = t.elapsed().as_millis();
    std::thread::sleep(Duration::from_millis(300));
    let back = ab
        .get_image()
        .map(|i| (i.width, i.height, i.bytes.into_owned() == rgba));
    println!(
        "[bigimage] {kind}: snapshot {snap_ms} ms, restore {restore_ms} ms ({res:?}); read back (w, h, identical): {back:?}"
    );
}
