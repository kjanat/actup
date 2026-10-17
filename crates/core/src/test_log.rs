use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|e| std::io::Error::other(e.to_string()))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn logged<T>(run: impl FnOnce() -> T) -> Result<(T, String), Box<dyn std::error::Error>> {
    let buffer = Buffer::default();
    let writer = buffer.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .without_time()
        .with_target(false)
        .finish();
    let value = tracing::subscriber::with_default(subscriber, run);
    let bytes = buffer.0.lock().map_err(|e| e.to_string())?.clone();
    Ok((value, String::from_utf8(bytes)?))
}
