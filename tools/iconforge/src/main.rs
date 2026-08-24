//! Generátor ikon pro Anvil.
//!
//! Tvar je popsaný jednou v návrhovém prostoru 1000×1000 a rasterizuje se
//! přímo do cílové velikosti — díky analytickému antialiasingu tiny-skia
//! není potřeba supersampling. Z jednoho zdroje vypadne všechno, co Tauri
//! potřebuje na obou platformách:
//!
//! * `32x32.png`, `128x128.png`, `128x128@2x.png`, `icon.png` — sada pro `tauri.conf.json`
//! * `icon.ico` — Windows (16/32/48/64/128 jako DIB, 256 jako PNG)
//! * `icon.icns` — macOS (`ic11`…`ic10`, všechno PNG)
//!
//! Motiv: kovadlina s rozžhaveným polotovarem na líci. Barvy musí zůstat
//! v souladu s `src/styles/theme.css` — když se změní paleta appky, pustí
//! se tenhle generátor znovu a ikona se nerozejde.
//!
//! Spuštění: `cargo run --release --manifest-path tools/iconforge/Cargo.toml`

use std::path::{Path as FsPath, PathBuf};

use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, Path, PathBuilder, Pixmap, Point,
    SpreadMode, Transform,
};

// --- Paleta ---------------------------------------------------------------

fn bg() -> Color {
    Color::from_rgba8(0x16, 0x18, 0x1D, 255)
}
fn steel_light() -> Color {
    Color::from_rgba8(0xDC, 0xE6, 0xF2, 255)
}
fn steel_dark() -> Color {
    Color::from_rgba8(0x6E, 0x82, 0x99, 255)
}
fn hot_deep() -> Color {
    Color::from_rgba8(0xFF, 0x7A, 0x2F, 255)
}
fn hot_bright() -> Color {
    Color::from_rgba8(0xFF, 0xD8, 0x96, 255)
}

// --- Tvary ----------------------------------------------------------------

/// Silueta kovadliny: líc s převisem, roh vlevo, konkávní pas, rozšířená pata.
/// Souřadnice jsou v prostoru 1000×1000 a násobí se měřítkem `k`.
fn anvil_path(k: f32) -> Path {
    let u = |v: f32| v * k;
    let mut pb = PathBuilder::new();

    // Líc zleva doprava a dolů po pravé hraně
    pb.move_to(u(300.0), u(250.0));
    pb.line_to(u(862.0), u(250.0));
    pb.line_to(u(862.0), u(358.0));
    pb.line_to(u(660.0), u(358.0));
    // Pravý bok pasu — mírně konkávní, ne přeštípnutý
    pb.cubic_to(u(640.0), u(430.0), u(615.0), u(520.0), u(615.0), u(610.0));
    // Výběh do paty
    pb.cubic_to(u(615.0), u(662.0), u(720.0), u(655.0), u(828.0), u(700.0));
    pb.line_to(u(828.0), u(800.0));
    pb.line_to(u(172.0), u(800.0));
    pb.line_to(u(172.0), u(700.0));
    // Zrcadlově zpátky nahoru
    pb.cubic_to(u(280.0), u(655.0), u(385.0), u(662.0), u(385.0), u(610.0));
    pb.cubic_to(u(385.0), u(520.0), u(360.0), u(430.0), u(340.0), u(358.0));
    pb.line_to(u(300.0), u(358.0));
    // Roh: spodní hrana stoupá ke špičce, špička je tupá (jinak se v malých
    // velikostech ztratí do ostří), horní hrana se vrací skoro rovně k líci
    pb.cubic_to(u(245.0), u(366.0), u(180.0), u(353.0), u(135.0), u(330.0));
    pb.line_to(u(135.0), u(300.0));
    pb.cubic_to(u(195.0), u(279.0), u(243.0), u(255.0), u(300.0), u(250.0));
    pb.close();

    pb.finish().expect("silueta kovadliny je uzavřená")
}

/// Zaoblený obdélník. tiny-skia nemá `arc_to`, rohy se skládají z kubik
/// s obvyklou konstantou 4/3·(√2−1).
fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    const KAPPA: f32 = 0.552_284_75;
    let r = r.min(w / 2.0).min(h / 2.0);
    let c = r * KAPPA;
    let (x0, y0, x1, y1) = (x, y, x + w, y + h);

    let mut pb = PathBuilder::new();
    pb.move_to(x0 + r, y0);
    pb.line_to(x1 - r, y0);
    pb.cubic_to(x1 - r + c, y0, x1, y0 + r - c, x1, y0 + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + c, x1 - r + c, y1, x1 - r, y1);
    pb.line_to(x0 + r, y1);
    pb.cubic_to(x0 + r - c, y1, x0, y1 - r + c, x0, y1 - r);
    pb.line_to(x0, y0 + r);
    pb.cubic_to(x0, y0 + r - c, x0 + r - c, y0, x0 + r, y0);
    pb.close();

    pb.finish().expect("zaoblený obdélník je uzavřený")
}

// --- Vykreslení -----------------------------------------------------------

fn render(size: u32) -> Pixmap {
    let mut pm = Pixmap::new(size, size).expect("nenulová velikost ikony");
    let k = size as f32 / 1000.0;
    let u = |v: f32| v * k;

    let mut paint = Paint::default();
    paint.anti_alias = true;

    // Podklad — zaoblený čtverec přes celou plochu
    paint.set_color(bg());
    pm.fill_path(
        &rounded_rect(u(20.0), u(20.0), u(960.0), u(960.0), u(210.0)),
        &paint,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    // Rozžhavený polotovar na líci. Pod 32 px by z něj i s aureolou zbyl jen
    // oranžový šmouh přes horní třetinu, takže se celý vynechá.
    if size >= 32 {
        // Aureola: tři obálky s klesající krycí schopností místo skutečného
        // rozostření — tiny-skia blur nemá a konvoluce je tu zbytečná.
        for (grow, alpha) in [
            (52.0_f32, 11_u8),
            (40.0, 16),
            (29.0, 23),
            (19.0, 32),
            (10.0, 46),
        ] {
            let mut halo = Paint::default();
            halo.anti_alias = true;
            halo.set_color(Color::from_rgba8(0xFF, 0x7A, 0x2F, alpha));
            pm.fill_path(
                &rounded_rect(
                    u(440.0 - grow),
                    u(190.0 - grow),
                    u(350.0 + 2.0 * grow),
                    u(68.0 + 2.0 * grow),
                    u(34.0 + grow),
                ),
                &halo,
                FillRule::Winding,
                Transform::identity(),
                None,
            );
        }

        let mut bar = Paint::default();
        bar.anti_alias = true;
        bar.shader = LinearGradient::new(
            Point::from_xy(u(440.0), u(190.0)),
            Point::from_xy(u(790.0), u(258.0)),
            vec![
                GradientStop::new(0.0, hot_deep()),
                GradientStop::new(1.0, hot_bright()),
            ],
            SpreadMode::Pad,
            Transform::identity(),
        )
        .expect("gradient polotovaru má dva různé body");
        pm.fill_path(
            &rounded_rect(u(440.0), u(190.0), u(350.0), u(68.0), u(34.0)),
            &bar,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }

    // Kovadlina — svislý ocelový gradient, světlejší u líce
    let mut steel = Paint::default();
    steel.anti_alias = true;
    steel.shader = LinearGradient::new(
        Point::from_xy(0.0, u(240.0)),
        Point::from_xy(0.0, u(820.0)),
        vec![
            GradientStop::new(0.0, steel_light()),
            GradientStop::new(1.0, steel_dark()),
        ],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .expect("ocelový gradient má dva různé body");
    pm.fill_path(
        &anvil_path(k),
        &steel,
        FillRule::Winding,
        Transform::identity(),
        None,
    );

    pm
}

// --- Kontejnery -----------------------------------------------------------

/// 32bitový bottom-up DIB s prázdnou AND maskou — formát klasické položky ICO.
///
/// Průhlednost nese alfa kanál, maska zůstává vynulovaná. Menší velikosti se
/// ukládají takhle a ne jako PNG, protože PNG-in-ICO některé starší nástroje
/// (a GDI+) nepřečtou.
fn dib_bytes(pm: &Pixmap) -> Vec<u8> {
    let s = pm.width() as usize;
    let px = pm.pixels();

    let mut xor = vec![0u8; s * s * 4];
    for y in 0..s {
        let src_row = s - 1 - y; // DIB je uložený zdola nahoru
        for x in 0..s {
            let c = px[src_row * s + x].demultiply();
            let i = (y * s + x) * 4;
            xor[i] = c.blue();
            xor[i + 1] = c.green();
            xor[i + 2] = c.red();
            xor[i + 3] = c.alpha();
        }
    }

    let mask_stride = (s + 31) / 32 * 4;
    let mask = vec![0u8; mask_stride * s];

    let mut out = Vec::with_capacity(40 + xor.len() + mask.len());
    out.extend_from_slice(&40u32.to_le_bytes()); // biSize
    out.extend_from_slice(&(s as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&((s as i32) * 2).to_le_bytes()); // biHeight = obraz + maska
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&((xor.len() + mask.len()) as u32).to_le_bytes());
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
    out.extend_from_slice(&xor);
    out.extend_from_slice(&mask);
    out
}

fn ico_bytes(entries: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // typ 1 = ikona
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());

    let mut offset = 6 + 16 * entries.len() as u32; // hlavička + adresář
    for (size, data) in entries {
        let dim = if *size >= 256 { 0u8 } else { *size as u8 }; // 0 v ICO znamená 256
        out.push(dim); // šířka
        out.push(dim); // výška
        out.push(0); // barev v paletě
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // color planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bitů na pixel
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in entries {
        out.extend_from_slice(data);
    }
    out
}

/// ICNS je plochý seznam typovaných bloků. Moderní macOS čte `ic07`–`ic14`
/// jako PNG, takže se nic nepřevádí — jen se obalí hlavičkou.
/// Délka v hlavičce bloku **zahrnuje** těch 8 bajtů hlavičky.
fn icns_bytes(entries: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (kind, data) in entries {
        body.extend_from_slice(*kind);
        body.extend_from_slice(&((data.len() + 8) as u32).to_be_bytes());
        body.extend_from_slice(data);
    }

    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"icns");
    out.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

// --- Běh ------------------------------------------------------------------

fn out_dir() -> PathBuf {
    // tools/iconforge → ../../src-tauri/icons
    FsPath::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("src-tauri")
        .join("icons")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let dir = out_dir();
    std::fs::create_dir_all(&dir)?;

    // Sada, kterou očekává tauri.conf.json → bundle.icon
    for (name, size) in [
        ("32x32.png", 32u32),
        ("128x128.png", 128),
        ("128x128@2x.png", 256),
        ("icon.png", 1024),
    ] {
        let png = render(size).encode_png()?;
        std::fs::write(dir.join(name), &png)?;
        println!("  {name} ({size}px, {} B)", png.len());
    }

    // Windows
    let mut ico = Vec::new();
    for size in [16u32, 32, 48, 64, 128] {
        ico.push((size, dib_bytes(&render(size))));
    }
    ico.push((256, render(256).encode_png()?)); // 256 jako PNG kvůli velikosti
    let ico = ico_bytes(&ico);
    std::fs::write(dir.join("icon.ico"), &ico)?;
    println!("  icon.ico (6 velikostí, {} B)", ico.len());

    // macOS. Dvojice typ ↔ velikost: ic11 = 16@2x, ic12 = 32@2x,
    // ic07 = 128, ic13 = 128@2x, ic08 = 256, ic14 = 256@2x, ic09 = 512, ic10 = 512@2x.
    let icns_plan: [(&[u8; 4], u32); 8] = [
        (b"ic11", 32),
        (b"ic12", 64),
        (b"ic07", 128),
        (b"ic13", 256),
        (b"ic08", 256),
        (b"ic14", 512),
        (b"ic09", 512),
        (b"ic10", 1024),
    ];
    let mut icns = Vec::new();
    for (kind, size) in icns_plan {
        icns.push((kind, render(size).encode_png()?));
    }
    let icns = icns_bytes(&icns);
    std::fs::write(dir.join("icon.icns"), &icns)?;
    println!("  icon.icns (8 bloků, {} B)", icns.len());

    println!("hotovo: {}", dir.display());
    Ok(())
}
