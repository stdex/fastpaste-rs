//! Renders a window to a PNG-able buffer with no display attached, so the
//! layout can actually be looked at.
//!
//! The app has no other way to show its own UI on a headless machine, and
//! "does this look right" is not a question the type checker answers.
//!
//!     cargo run --example ui_preview -- options 2 out.ppm
//!
//! Writes a binary PPM (P6) — no image-encoder dependency; convert with
//! `ffmpeg -i out.ppm out.png`.

use std::rc::Rc;

use slint::platform::software_renderer::{
    MinimalSoftwareWindow, PremultipliedRgbaColor, RepaintBufferType, TargetPixel,
};
use slint::platform::{Platform, WindowAdapter};

slint::include_modules!();

struct PreviewPlatform {
    window: Rc<MinimalSoftwareWindow>,
}

impl Platform for PreviewPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.window.clone())
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let which = args.next().unwrap_or_else(|| "options".into());
    let page: i32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let out = args.next().unwrap_or_else(|| "preview.ppm".into());

    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(PreviewPlatform {
        window: window.clone(),
    }))?;

    let (w, h) = match which.as_str() {
        "select" => (400u32, 340u32),
        "main" => (900, 520),
        _ => (700, 460),
    };

    // Keep the component alive for the whole render.
    let _keep: Box<dyn std::any::Any> = match which.as_str() {
        "options" => Box::new(build_options(page)?),
        "select" => Box::new(build_selection()?),
        _ => Box::new(build_main_with(page == 1)?),
    };

    window.set_size(slint::PhysicalSize::new(w, h));
    slint::platform::update_timers_and_animations();

    let mut buf = vec![PremultipliedRgbaColor::from_rgb(255, 255, 255); (w * h) as usize];
    window.request_redraw();
    window.draw_if_needed(|renderer| {
        renderer.render(&mut buf, w as usize);
    });

    let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
    for px in &buf {
        ppm.extend_from_slice(&[px.red, px.green, px.blue]);
    }
    std::fs::write(&out, ppm)?;
    eprintln!("wrote {out} ({w}x{h})");
    Ok(())
}

fn build_options(page: i32) -> Result<OptionsDialog, slint::PlatformError> {
    let d = OptionsDialog::new()?;
    let langs = [
        "System default",
        "English",
        "Русский",
        "Deutsch",
        "Español",
        "中文 (简体)",
    ];
    d.set_languages(slint::ModelRc::new(slint::VecModel::from(
        langs
            .iter()
            .map(|l| LanguageOption {
                code: (*l).into(),
                label: (*l).into(),
            })
            .collect::<Vec<_>>(),
    )));
    d.set_language_labels(slint::ModelRc::new(slint::VecModel::from(
        langs
            .iter()
            .map(|l| slint::SharedString::from(*l))
            .collect::<Vec<_>>(),
    )));
    // Russian strings: the longest labels the app ships, and the ones the
    // fixed-width label gutter has to survive.
    let t = Translations::get(&d);
    t.set_options_title("Настройки".into());
    t.set_options_general("Общие".into());
    t.set_options_hotkeys("Горячие клавиши".into());
    t.set_options_clipboard_history("История буфера обмена".into());
    t.set_options_paste("Параметры вставки".into());
    t.set_options_language_label("Язык:".into());
    t.set_options_language_hint(
        "Выберите язык приложения. «Системный» — использовать язык операционной системы.".into(),
    );
    t.set_options_open_dialog_label("Открыть диалог выбора:".into());
    t.set_options_open_main_window_label("Открыть главное окно:".into());
    t.set_options_hotkeys_hint(
        "Модификаторы: Ctrl, Alt, Shift, Super. Объединяются через «+». Пример: Ctrl+U".into(),
    );
    t.set_options_capture_history("Отслеживать изменения буфера обмена".into());
    t.set_options_max_items_label("Макс. элементов:".into());
    t.set_options_folder_position_label("Положение папки:".into());
    t.set_options_position_top("Сверху".into());
    t.set_options_position_bottom("Снизу".into());
    t.set_options_paste_delay_label("Задержка вставки (мс):".into());
    t.set_options_restore_clipboard(
        "Восстанавливать содержимое буфера обмена после вставки".into(),
    );
    t.set_options_ok("ОК".into());
    t.set_options_cancel("Отмена".into());
    t.set_options_apply("Применить".into());

    d.set_language_index(2);
    d.set_hotkey_open_dialog("Ctrl+U".into());
    d.set_hotkey_open_main_window("Ctrl+Shift+U".into());
    d.set_history_enabled(true);
    d.set_history_max_items(10);
    d.set_history_max_items_min(1);
    d.set_history_max_items_max(500);
    d.set_paste_delay_ms(70);
    d.set_paste_delay_min(0);
    d.set_paste_delay_max(5000);
    d.set_paste_restore_clipboard(true);
    d.set_active_page(page);
    d.show()?;
    Ok(d)
}

fn build_selection() -> Result<SelectionDialog, slint::PlatformError> {
    let d = SelectionDialog::new()?;
    let rows: Vec<SnippetRow> = [
        ("git status --short", "", "История"),
        ("cargo test --workspace", "", "История"),
        ("Адрес электронной почты", "user@example.com", ""),
        ("Подпись", "С уважением, команда fastpaste", ""),
        (
            "SQL: выборка",
            "SELECT id, title FROM items ORDER BY order_index;",
            "",
        ),
        (
            "Лицензия MIT",
            "Permission is hereby granted, free of charge, to any person",
            "",
        ),
    ]
    .iter()
    .map(|(t, b, tag)| SnippetRow {
        title: (*t).into(),
        body: (*b).into(),
        tag: (*tag).into(),
    })
    .collect();
    d.set_snippets(slint::ModelRc::new(slint::VecModel::from(rows)));
    let t = Translations::get(&d);
    t.set_selection_dialog_title("Выберите строку для вставки".into());
    t.set_selection_filter_placeholder("Введите для фильтрации".into());
    t.set_selection_hint("↑↓ выбрать · Enter вставить · Esc закрыть".into());
    t.set_selection_tag_history("История".into());
    d.show()?;
    Ok(d)
}

fn build_main_with(confirm: bool) -> Result<MainWindow, slint::PlatformError> {
    use slint_tree_view::TreeItem;
    let w = MainWindow::new()?;

    // Mirror the palette the app installs in `build_main_window`. Without
    // it the preview renders the TreeView's stock style, so it would be
    // showing something the user never sees — which makes it useless for
    // exactly the question it exists to answer.
    {
        use slint::Global;
        let rgb = slint::Color::from_rgb_u8;
        let style = slint_tree_view::TreeViewStyle::get(&w);
        style.set_background_color(rgb(0xff, 0xff, 0xff));
        style.set_text_color(rgb(0x21, 0x25, 0x29));
        style.set_highlight_color(rgb(0xdb, 0xea, 0xfe));
        style.set_highlighted_text_color(rgb(0xff, 0xff, 0xff));
        style.set_hover_color(rgb(0xe9, 0xec, 0xef));
        style.set_branch_indicator_color(rgb(0x6a, 0x73, 0x7d));
    }
    let t = Translations::get(&w);
    t.set_toolbar_add_folder("📁 Папка".into());
    t.set_toolbar_add_snippet("📄 Фрагмент".into());
    t.set_toolbar_delete("🗑 Удалить".into());
    t.set_editor_title_label("Заголовок:".into());
    t.set_editor_body_label("Текст:".into());

    let mut rows: Vec<TreeItem> = Vec::new();
    let mut folder = TreeItem::branch(1, -1, 0, "Рабочие заметки")
        .with_icon("📁")
        .with_item_type(0);
    folder.has_children = true;
    folder.expanded = true;
    rows.push(folder);
    rows.push(
        TreeItem::leaf(2, 1, 1, "Приветствие", "Здравствуйте!")
            .with_icon("📄")
            .with_item_type(1),
    );
    rows.push(
        TreeItem::leaf(3, 1, 1, "Реквизиты", "ИНН 7701234567")
            .with_icon("📄")
            .with_item_type(1),
    );
    rows.push(
        TreeItem::leaf(4, -1, 0, "Подпись", "С уважением")
            .with_icon("📄")
            .with_item_type(1),
    );
    let mut hist = TreeItem::branch(-1000, -1, 0, "История буфера обмена")
        .with_icon("🕒")
        .with_item_type(0);
    hist.has_children = true;
    hist.expanded = true;
    rows.push(hist);
    rows.push(
        TreeItem::leaf(
            -2,
            -1000,
            1,
            "git commit -m \"fix\"",
            "git commit -m \"fix\"",
        )
        .with_icon("📄")
        .with_item_type(1),
    );
    w.set_tree_model(slint::ModelRc::new(slint::VecModel::from(rows)));
    w.set_current_index(1);
    w.set_editor_title("Приветствие".into());
    w.set_editor_body("Здравствуйте!\n\nСпасибо за обращение.".into());
    w.set_editor_enabled(true);
    w.set_title_enabled(true);
    if confirm {
        let t = Translations::get(&w);
        t.set_confirm_delete_title("Удалить?".into());
        t.set_confirm_yes("Удалить".into());
        t.set_confirm_no("Отмена".into());
        w.set_confirm_delete_message(
            "Будет безвозвратно удалена эта папка и всё её содержимое:\n\nРабочие заметки".into(),
        );
        w.set_confirm_delete_visible(true);
    }
    w.show()?;
    Ok(w)
}
