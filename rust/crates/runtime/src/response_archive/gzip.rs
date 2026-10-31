//! Reuse finished compression state without retaining a session or file sink.
use flate2::{write::DeflateEncoder, Compression, Crc};
use std::io::{self, Write};
use std::sync::Mutex;

struct Output(Option<Box<dyn Write + Send + Sync>>);

impl Write for Output {
    #[inline]
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        // A detached, finished deflater uses a sink during cleanup/reset only.
        self.0
            .as_mut()
            .map_or(Ok(bytes.len()), |writer| writer.write(bytes))
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.0.as_mut().map_or(Ok(()), |writer| writer.flush())
    }
}

type Deflater = DeflateEncoder<Output>;
// Concurrent captures own separate states; retain at most one idle state.
static POOL: Mutex<Option<Deflater>> = Mutex::new(None);

pub(super) struct Encoder {
    deflater: Option<Deflater>,
    crc: Crc,
    header: bool,
    finalized: bool,
}

impl Encoder {
    #[inline]
    pub(super) fn new(writer: impl Write + Send + Sync + 'static) -> io::Result<Self> {
        let cached = POOL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let output = Output(Some(Box::new(writer)));
        let deflater = match cached {
            Some(mut deflater) => {
                deflater.reset(output)?;
                deflater
            }
            None => DeflateEncoder::new(output, Compression::fast()),
        };
        Ok(Self {
            deflater: Some(deflater),
            crc: Crc::new(),
            header: false,
            finalized: false,
        })
    }

    #[inline]
    fn header(&mut self) -> io::Result<()> {
        if !self.header {
            // RFC 1952: deflate, no optional fields, zero mtime, fast, unknown OS.
            self.deflater
                .as_mut()
                .unwrap()
                .get_mut()
                .write_all(&[31, 139, 8, 0, 0, 0, 0, 0, 4, 255])?;
            self.header = true;
        }
        Ok(())
    }

    #[inline]
    fn finalize(&mut self) -> io::Result<()> {
        if self.finalized {
            return Ok(());
        }
        self.finalized = true;
        self.header()?;
        let deflater = self.deflater.as_mut().unwrap();
        deflater.try_finish()?;
        let mut trailer = [0; 8];
        trailer[..4].copy_from_slice(&self.crc.sum().to_le_bytes());
        trailer[4..].copy_from_slice(&self.crc.amount().to_le_bytes());
        deflater.get_mut().write_all(&trailer)
    }

    #[inline]
    pub(super) fn finish(mut self) -> io::Result<()> {
        self.finalize()
    }
}

impl Write for Encoder {
    #[inline]
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.header()?;
        let count = self.deflater.as_mut().unwrap().write(bytes)?;
        self.crc.update(&bytes[..count]);
        Ok(count)
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.header()?;
        self.deflater.as_mut().unwrap().flush()
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        let _ = self.finalize();
        if let Some(mut deflater) = self.deflater.take() {
            // Preserve flate2's deflater cleanup when header/trailer I/O failed.
            let _ = deflater.try_finish();
            // Release the backend, budget and attachment lease before caching.
            drop(deflater.get_mut().0.take());
            if deflater.try_finish().is_ok() {
                let mut cached = POOL
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if cached.is_none() {
                    *cached = Some(deflater);
                }
            }
        }
    }
}
