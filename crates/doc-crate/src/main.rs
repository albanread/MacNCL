//! doc-crate — testing-first front-end over the `docpane` render core.
//!
//! Primary mode (today): a **headless, full-height snapshot**. Given a
//! markdown file, it lays the whole document out, renders it to an
//! offscreen WIC bitmap sized to the *entire* content height (no window,
//! no flash, deterministic), and writes a PNG. Reviewing a whole page is
//! then one image, and snapshot diffing is straightforward.
//!
//!   doc-crate --testsnap <file.md> [--width <px>] [--out <png>]
//!
//! It shares `docpane` with the in-window doc-pane, so what this renders
//! is exactly what the IDE pane will.

use std::path::PathBuf;

use windows::core::*;
use windows::Win32::Foundation::{E_FAIL, GENERIC_WRITE};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;

use docpane::{layout, parser, render, theme};

struct Args {
    file: PathBuf,
    width: f32,
    out: PathBuf,
}

fn usage() -> String {
    "usage: doc-crate --testsnap <file.md> [--width <px>] [--out <png>]".into()
}

fn parse_args() -> std::result::Result<Args, String> {
    let mut a = std::env::args().skip(1);
    if a.next().as_deref() != Some("--testsnap") {
        return Err(usage());
    }
    let file = a.next().map(PathBuf::from).ok_or_else(usage)?;
    let mut width = 860.0_f32;
    let mut out = PathBuf::from("screen.png");
    while let Some(flag) = a.next() {
        match flag.as_str() {
            "--width" => {
                width = a
                    .next()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(|| format!("--width needs a number\n{}", usage()))?;
            }
            "--out" => {
                out = a.next().map(PathBuf::from).ok_or_else(usage)?;
            }
            other => return Err(format!("unknown option `{other}`\n{}", usage())),
        }
    }
    Ok(Args { file, width, out })
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    match run(&args) {
        Ok((w, h)) => {
            println!("wrote {} ({w}x{h})", args.out.display());
        }
        Err(e) => {
            eprintln!("doc-crate: {e}");
            std::process::exit(1);
        }
    }
}

fn run(args: &Args) -> Result<(u32, u32)> {
    let md = std::fs::read_to_string(&args.file).map_err(|e| {
        Error::new(E_FAIL, format!("read {}: {e}", args.file.display()))
    })?;

    unsafe {
        // STA COM for WIC + the single-threaded D2D factory.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        render::init()?; // D2D + DWrite factories (needed by measure_text)

        // Lay the whole document out at the requested width.  Content
        // sits inside an H_PAD margin on each side; the image is the
        // full requested width by the full content height.
        let x_base = theme::H_PAD;
        let content_w = (args.width - 2.0 * theme::H_PAD).max(1.0);
        let blocks = parser::parse(&md);
        let doc = layout::layout(&blocks, x_base, content_w, 0.0, render::measure_text);
        let width_px = args.width.ceil().max(1.0) as u32;
        let height_px = (doc.total_h + theme::V_PAD).ceil().max(1.0) as u32;

        // Offscreen WIC bitmap + a Direct2D render target over it.
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let bmp = wic.CreateBitmap(
            width_px,
            height_px,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapCacheOnLoad,
        )?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = render::factory().CreateWicBitmapRenderTarget(&bmp, &props)?;

        rt.BeginDraw();
        let bg = theme::hex(theme::BG);
        rt.Clear(Some(std::ptr::addr_of!(bg)));
        // The whole document is on-screen: viewport height = content
        // height, scroll = 0, so nothing is clipped.
        render::draw_document(&rt, &doc, 0.0, height_px as f32)?;
        rt.EndDraw(None, None)?;

        save_png(&wic, &bmp, &args.out, width_px, height_px)?;
        Ok((width_px, height_px))
    }
}

/// Encode a WIC bitmap to a PNG file (atomic via a .tmp rename).
unsafe fn save_png(
    wic: &IWICImagingFactory,
    bmp: &IWICBitmap,
    path: &std::path::Path,
    width: u32,
    height: u32,
) -> Result<()> {
    let tmp = path.with_extension("png.tmp");
    let _ = std::fs::remove_file(&tmp);
    {
        let stream = wic.CreateStream()?;
        let wide: Vec<u16> = tmp
            .to_string_lossy()
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        stream.InitializeFromFilename(PCWSTR(wide.as_ptr()), GENERIC_WRITE.0)?;

        let encoder = wic.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
        encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;

        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
        let frame = frame.ok_or_else(|| Error::new(E_FAIL, "png encoder gave no frame"))?;
        frame.Initialize(None)?;
        frame.SetSize(width, height)?;
        let mut fmt = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut fmt)?;
        frame.WriteSource(bmp, std::ptr::null())?;
        frame.Commit()?;
        encoder.Commit()?;
    }
    let _ = std::fs::remove_file(path);
    std::fs::rename(&tmp, path)
        .map_err(|e| Error::new(E_FAIL, format!("rename {}: {e}", path.display())))?;
    Ok(())
}
