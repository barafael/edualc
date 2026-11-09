//! What GPUI loads by path: edualc's heron, as `heron/<drawing>.svg` (the Slint window's
//! drawings), and Lucide's icons, as `icons/<name>.svg`, the whole catalog (gpui-kit-assets'),
//! which gpui-component's own icons are part of.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString};

pub(crate) struct Assets;

/// The heron's drawings (`ui::heron`), by name.
const HERON: &[(&str, &[u8])] = &[
    ("heron", include_bytes!("../../../../assets/heron.svg")),
    (
        "heron-wings-up",
        include_bytes!("../../../../assets/heron-wings-up.svg"),
    ),
    (
        "heron-wings-down",
        include_bytes!("../../../../assets/heron-wings-down.svg"),
    ),
    (
        "heron-wings-level",
        include_bytes!("../../../../assets/heron-wings-level.svg"),
    ),
    (
        "heron-looking-back",
        include_bytes!("../../../../assets/heron-looking-back.svg"),
    ),
    (
        "heron-preening-1",
        include_bytes!("../../../../assets/heron-preening-1.svg"),
    ),
    (
        "heron-preening-2",
        include_bytes!("../../../../assets/heron-preening-2.svg"),
    ),
    (
        "heron-flapping-half",
        include_bytes!("../../../../assets/heron-flapping-half.svg"),
    ),
    (
        "heron-flapping-up",
        include_bytes!("../../../../assets/heron-flapping-up.svg"),
    ),
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> gpui::Result<Option<Cow<'static, [u8]>>> {
        let heron = path
            .strip_prefix("heron/")
            .and_then(|file| file.strip_suffix(".svg"))
            .and_then(|name| HERON.iter().find(|(drawing, _)| *drawing == name));
        if let Some((_, bytes)) = heron {
            return Ok(Some(Cow::Borrowed(bytes)));
        }
        gpui_kit_assets::AllAssets.load(path)
    }

    fn list(&self, path: &str) -> gpui::Result<Vec<SharedString>> {
        gpui_kit_assets::AllAssets.list(path)
    }
}
