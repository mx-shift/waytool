//! GNOME's layout mode, from mutter's org.gnome.Mutter.DisplayConfig
//! interface (documented in its D-Bus XML) — the one thing about the
//! layout the Wayland protocols don't say (see `wayland` for the rest).
//! GNOME-only: elsewhere the call fails and the default, logical, is
//! assumed.

use std::collections::HashMap;

use anyhow::{Context, Result};
use zbus::OwnedVariant;

const DEST: &str = "org.gnome.Mutter.DisplayConfig";
const PATH: &str = "/org/gnome/Mutter/DisplayConfig";

/// GetCurrentState's reply: serial, monitors, logical monitors,
/// properties.
type CurrentState = (
    u32,
    Vec<(
        (String, String, String, String),
        Vec<(
            String,
            i32,
            i32,
            f64,
            f64,
            Vec<f64>,
            HashMap<String, OwnedVariant>,
        )>,
        HashMap<String, OwnedVariant>,
    )>,
    Vec<(
        i32,
        i32,
        f64,
        u32,
        bool,
        Vec<(String, String, String, String)>,
        HashMap<String, OwnedVariant>,
    )>,
    HashMap<String, OwnedVariant>,
);

/// Whether GNOME lays monitors out in physical pixels ("layout-mode"
/// 2): each one as big as its mode, whatever its scale, so a screenshot
/// pixel is one logical pixel. In the default logical mode a monitor's
/// size is its mode divided by its scale. Absent means logical
/// (DisplayConfig docs).
pub(crate) async fn physical_layout(bus: &zbus::Connection) -> Result<bool> {
    let reply = bus
        .call_method(Some(DEST), PATH, Some(DEST), "GetCurrentState", &())
        .await
        .context("DisplayConfig.GetCurrentState")?;
    let (_serial, _monitors, _logical, properties): CurrentState = reply
        .body()
        .deserialize()
        .context("DisplayConfig.GetCurrentState reply")?;
    Ok(properties
        .get("layout-mode")
        .and_then(|v| u32::try_from(v).ok())
        == Some(2))
}

#[cfg(test)]
mod tests {
    /// Live: reads the running GNOME session's layout mode.
    /// `cargo test -- --ignored layout_mode_live --nocapture`
    #[tokio::test]
    #[ignore]
    async fn layout_mode_live() {
        let bus = zbus::Connection::session().await.unwrap();
        println!("physical: {}", super::physical_layout(&bus).await.unwrap());
    }
}
