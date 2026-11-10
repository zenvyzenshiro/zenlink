//! Copy text from the device's screen (Alt+C) with Windows' built-in OCR (WinRT OcrEngine).

#[cfg(windows)]
mod imp {
    use std::sync::Mutex;
    use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
    use windows::Media::Ocr::OcrEngine;
    use windows::Storage::Streams::DataWriter;

    static LINES: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn recognize(width: u32, height: u32, bgra: &[u8]) -> windows::core::Result<Vec<String>> {
        let w = DataWriter::new()?;
        w.WriteBytes(bgra)?;
        let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
            &w.DetachBuffer()?,
            BitmapPixelFormat::Bgra8,
            width as i32,
            height as i32,
        )?;
        let engine = OcrEngine::TryCreateFromUserProfileLanguages()?;
        let result = engine.RecognizeAsync(&bitmap)?.join()?;
        let mut out = Vec::new();
        for line in result.Lines()? {
            let t = line.Text()?.to_string();
            if !t.trim().is_empty() {
                out.push(t.trim_end().to_string());
            }
        }
        Ok(out)
    }

    fn norm(line: &str) -> String {
        line.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    }

    fn merge(old: &mut Vec<String>, new: Vec<String>) -> usize {
        let a: Vec<String> = old.iter().map(|x| norm(x)).collect();
        let b: Vec<String> = new.iter().map(|x| norm(x)).collect();
        let best = (1..=a.len().min(b.len()))
            .rev()
            .find(|&k| a[a.len() - k..] == b[..k])
            .unwrap_or(0);
        if !old.is_empty() && best == 0 && !b.is_empty() && b.iter().all(|x| a.contains(x)) {
            return 0;
        }
        let added = new.len() - best;
        old.extend(new.into_iter().skip(best));
        added
    }

    pub fn add_capture(width: u32, height: u32, bgra: &[u8]) -> Result<(String, usize, usize), String> {
        let lines = recognize(width, height, bgra).map_err(|e| e.message().to_string())?;
        let read = lines.len();
        let mut all = LINES.lock().unwrap();
        let added = merge(&mut all, lines);
        log::info!(
            "ocr: {read} lines read, {added} new, {} stitched",
            all.len()
        );
        Ok((all.join("\n"), added, all.len()))
    }

    pub fn clear() {
        LINES.lock().unwrap().clear();
    }
}

#[cfg(windows)]
pub use imp::{add_capture, clear};

#[cfg(not(windows))]
pub fn add_capture(_width: u32, _height: u32, _bgra: &[u8]) -> Result<(String, usize, usize), String> {
    Err("OCR is not available on Linux yet".into())
}

#[cfg(not(windows))]
pub fn clear() {}
