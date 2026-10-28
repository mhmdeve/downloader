use std::{fs, path::Path};

fn main() {
    // Tauri expects platform icon assets even during `cargo test`. Generate
    // valid placeholder files for clean checkouts; replace with real artwork
    // before the first production release.
    let icons = Path::new("icons");
    fs::create_dir_all(icons).expect("create Tauri icons directory");

    // 1x1 transparent PNG.
    let png: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A,
        0x00, 0x00, 0x00, 0x0D, b'I', b'H', b'D', b'R',
        0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
        0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89,
        0x00, 0x00, 0x00, 0x0B, b'I', b'D', b'A', b'T',
        0x78, 0xDA, 0x63, 0x60, 0x00, 0x02, 0x00, 0x00, 0x05, 0x00, 0x01,
        0xA5, 0xF6, 0x45, 0x40,
        0x00, 0x00, 0x00, 0x00, b'I', b'E', b'N', b'D', 0xAE, 0x42, 0x60, 0x82,
    ];
    fs::write(icons.join("icon.png"), png).expect("write placeholder PNG icon");

    // ICO file containing the PNG image (header + one directory entry + PNG).
    let mut ico = vec![0, 0, 1, 0, 1, 0, 1, 1, 0, 0, 1, 0, 32, 0];
    ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
    ico.extend_from_slice(&22u32.to_le_bytes());
    ico.extend_from_slice(png);
    fs::write(icons.join("icon.ico"), ico).expect("write placeholder ICO icon");

    // ICNS container with a PNG-backed icon entry. This placeholder is only
    // intended to satisfy development/CI; production artwork will replace it.
    let mut icns = b"icns".to_vec();
    icns.extend_from_slice(&((16 + png.len()) as u32).to_be_bytes());
    icns.extend_from_slice(b"icp4");
    icns.extend_from_slice(&((8 + png.len()) as u32).to_be_bytes());
    icns.extend_from_slice(png);
    fs::write(icons.join("icon.icns"), icns).expect("write placeholder ICNS icon");

    tauri_build::build()
}
