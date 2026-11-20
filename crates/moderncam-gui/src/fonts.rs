//! System CJK font fallback registration compatible with `egui-elegance`.
//!
//! Uses `egui::Context::add_font` with `FontPriority::Lowest` instead of `set_fonts`
//! so `elegance`'s bundled symbol/icon font and egui defaults are preserved.

use std::path::Path;

use egui::{
    Context, FontData, FontFamily,
    epaint::text::{FontInsert, FontPriority, InsertFontFamily},
};

const CANDIDATE_CJK_FONTS: &[&str] = &[
    // macOS Simplified Chinese system fonts
    "/System/Library/Fonts/PingFang.ttc",
    "/System/Library/Fonts/STHeiti Medium.ttc",
    "/System/Library/Fonts/Hiragino Sans GB.ttc",
    "/System/Library/Fonts/Supplemental/Songti.ttc",
    // Windows Simplified Chinese system fonts
    r"C:\Windows\Fonts\msyh.ttc",
    r"C:\Windows\Fonts\simhei.ttf",
    r"C:\Windows\Fonts\simsun.ttc",
    // Linux CJK system fonts
    "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/google-noto-cjk/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
    "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
    "/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc",
];

/// Attempts to register a system CJK fallback font into `ctx` without replacing existing fonts.
pub fn install_cjk_fallback(ctx: &Context) {
    for candidate in CANDIDATE_CJK_FONTS {
        let path = Path::new(candidate);
        if !path.is_file() {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        ctx.add_font(FontInsert::new(
            "system_cjk_fallback",
            FontData::from_owned(bytes),
            vec![
                InsertFontFamily {
                    family: FontFamily::Proportional,
                    priority: FontPriority::Lowest,
                },
                InsertFontFamily {
                    family: FontFamily::Monospace,
                    priority: FontPriority::Lowest,
                },
            ],
        ));
        log::info!("Registered CJK fallback font from {}", path.display());
        return;
    }
    log::warn!(
        "No system CJK font file found in standard locations; relying on eframe system_fonts"
    );
}
