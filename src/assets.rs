//! Embedded UI assets (SVG icons), served to gpui via [`AssetSource`].
//! Icons render tinted with the element's text color.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};

const ICONS: &[(&str, &[u8])] = &[(
    "icons/pencil.svg",
    include_bytes!("../assets/icons/pencil.svg"),
)];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS
            .iter()
            .find(|(name, _)| *name == path)
            .map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS
            .iter()
            .filter(|(name, _)| name.starts_with(path))
            .map(|(name, _)| SharedString::from(*name))
            .collect())
    }
}
