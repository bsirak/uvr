use anyhow::Result;

use uvr_core::r_version::detector::find_all;
use uvr_core::r_version::downloader::{fetch_available_versions, Platform};

use crate::ui;
use crate::ui::palette;

pub async fn run(all: bool) -> Result<()> {
    let installations = find_all();

    if all {
        let client = crate::commands::util::build_client()?;
        let platform =
            Platform::detect().map_err(|e| anyhow::anyhow!("Unsupported platform: {e}"))?;
        let available = fetch_available_versions(&client, platform)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to fetch available R versions: {e}"))?;

        let installed: std::collections::HashSet<&str> =
            installations.iter().map(|i| i.version.as_str()).collect();

        println!("{}", palette::bold("Available R versions"));

        // Channels first, and always tagged. They sit above the releases
        // because they track ahead of them — but they are not "newer than
        // 4.6.1", so they are never sorted into the list.
        for ch in &available.rolling {
            let tag = palette::warn("[unstable]");
            if installed.contains(ch.as_str()) {
                println!(
                    "  {} {} {} {}",
                    palette::success(ui::glyph::success()),
                    palette::info(ch),
                    tag,
                    palette::dim("[installed]"),
                );
            } else {
                println!(
                    "  {} {} {}",
                    palette::dim(ui::glyph::bullet()),
                    palette::dim(ch),
                    tag,
                );
            }
        }

        for ver in available.stable.iter().rev() {
            if installed.contains(ver.as_str()) {
                println!(
                    "  {} {} {}",
                    palette::success(ui::glyph::success()),
                    palette::info(ver),
                    palette::dim("[installed]"),
                );
            } else {
                println!(
                    "  {} {}",
                    palette::dim(ui::glyph::bullet()),
                    palette::dim(ver),
                );
            }
        }

        if !available.rolling.is_empty() {
            // Named from what the index actually carries: if Posit ever drops
            // one of the channels, a hardcoded pair would name a version that
            // is not on the list above it.
            let names = available
                .rolling
                .iter()
                .map(|c| format!("`{c}`"))
                .collect::<Vec<_>>()
                .join(" and ");
            ui::hint(format!(
                "{names} are rebuilt continuously — installing one pins a moving target, \
                 not a release."
            ));
        }
        return Ok(());
    }

    if installations.is_empty() {
        ui::warn("No R installations found.");
        ui::hint("Install R with: uvr r install <version>");
        return Ok(());
    }

    println!("{}", palette::bold("Installed R versions"));
    for inst in &installations {
        let tag = if inst.managed {
            palette::info("[uvr-managed]").to_string()
        } else {
            palette::dim(format!("[system: {}]", inst.binary.display())).to_string()
        };
        println!(
            "  {} {} {}",
            palette::success(ui::glyph::success()),
            palette::info(&inst.version),
            tag,
        );
    }

    Ok(())
}
