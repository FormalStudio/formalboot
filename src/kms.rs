use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, BorrowedFd};
use std::path::PathBuf;

use drm::buffer::{Buffer, DrmFourcc};
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{ClipRect, Device as ControlDevice, ModeTypeFlags, connector, crtc, framebuffer};

use crate::canvas::Canvas;

struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl ControlDevice for Card {}

struct Output {
    fb: framebuffer::Handle,
    buffer: DumbBuffer,
    width: usize,
    height: usize,
}

struct Head {
    path: PathBuf,
    card: Card,
    /// The connectors that were connected when `outputs` was set up.
    connected: Vec<connector::Handle>,
    outputs: Vec<Output>,
    /// Whether the last attempt to set `outputs` up went through.
    lit: bool,
}

impl Head {
    fn connected(&self) -> io::Result<Vec<connector::Handle>> {
        let resources = self.card.resource_handles()?;
        let mut out = Vec::new();
        for &handle in resources.connectors() {
            if self.card.get_connector(handle, false)?.state() == connector::State::Connected {
                out.push(handle);
            }
        }
        Ok(out)
    }

    /// One dumb buffer per connected connector at its preferred mode, each on a CRTC of its own.
    fn light(&mut self) -> io::Result<()> {
        self.release();
        let resources = self.card.resource_handles()?;
        let mut taken: Vec<crtc::Handle> = Vec::new();
        for &handle in &self.connected {
            let info = self.card.get_connector(handle, false)?;
            let Some(mode) = info
                .modes()
                .iter()
                .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
                .or(info.modes().first())
                .copied()
            else {
                continue;
            };
            let crtc = info.encoders().iter().find_map(|&e| {
                let encoder = self.card.get_encoder(e).ok()?;
                resources.filter_crtcs(encoder.possible_crtcs()).into_iter().find(|c| !taken.contains(c))
            });
            let Some(crtc) = crtc else { continue };
            let (w, h) = mode.size();
            let buffer = self.card.create_dumb_buffer((w.into(), h.into()), DrmFourcc::Xrgb8888, 32)?;
            let fb = match self.card.add_framebuffer(&buffer, 24, 32) {
                Ok(fb) => fb,
                Err(e) => {
                    let _ = self.card.destroy_dumb_buffer(buffer);
                    return Err(e);
                }
            };
            let output = Output { fb, buffer, width: w.into(), height: h.into() };
            // Kept even when the mode set fails, so `release` frees it and the next scan retries.
            let set = self.card.set_crtc(crtc, Some(fb), (0, 0), &[handle], Some(mode));
            self.outputs.push(output);
            set?;
            taken.push(crtc);
        }
        Ok(())
    }

    fn release(&mut self) {
        for output in self.outputs.drain(..) {
            let _ = self.card.destroy_framebuffer(output.fb);
            let _ = self.card.destroy_dumb_buffer(output.buffer);
        }
    }
}

/// Every display of every DRM card, lit as the cards and their connectors come and go.
#[derive(Default)]
pub struct Display {
    heads: Vec<Head>,
}

impl Display {
    /// Follows cards appearing (the real driver replacing simpledrm in the initrd), leaving, and
    /// connectors changing. Returns whether the set of outputs changed.
    pub fn scan(&mut self) -> bool {
        let mut changed = false;
        let mut paths: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("card")))
            .collect();
        paths.sort();
        let before = self.heads.len();
        self.heads.retain(|h| paths.contains(&h.path));
        changed |= self.heads.len() != before;
        for path in paths {
            if self.heads.iter().any(|h| h.path == path) {
                continue;
            }
            if let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) {
                self.heads.push(Head {
                    path,
                    card: Card(file),
                    connected: Vec::new(),
                    outputs: Vec::new(),
                    lit: false,
                });
            }
        }
        let mut gone = Vec::new();
        for (n, head) in self.heads.iter_mut().enumerate() {
            let Ok(connected) = head.connected() else {
                gone.push(n);
                continue;
            };
            // A card another master still holds (a compositor on its way out at shutdown) refuses
            // the mode set and is tried again on the next scan.
            if connected != head.connected || !head.lit {
                head.connected = connected;
                let was = head.outputs.len();
                head.lit = head.light().is_ok();
                if !head.lit {
                    head.release();
                }
                changed |= was != 0 || !head.outputs.is_empty();
            }
        }
        for n in gone.into_iter().rev() {
            self.heads.remove(n);
            changed = true;
        }
        changed
    }

    pub fn sizes(&self) -> Vec<(usize, usize)> {
        self.heads.iter().flat_map(|h| &h.outputs).map(|o| (o.width, o.height)).collect()
    }

    /// Copies `canvases[n]` to the nth output, in the order `sizes` gave.
    pub fn present(&mut self, canvases: &[Canvas]) {
        let mut n = 0;
        for head in &mut self.heads {
            for output in &mut head.outputs {
                let canvas = &canvases[n];
                n += 1;
                let pitch = output.buffer.pitch() as usize;
                let Ok(mut map) = head.card.map_dumb_buffer(&mut output.buffer) else { continue };
                for (row, src) in map.chunks_exact_mut(pitch).zip(canvas.px.chunks_exact(canvas.w)) {
                    for (dst, px) in row.as_chunks_mut::<4>().0.iter_mut().zip(src) {
                        dst.copy_from_slice(&px.to_le_bytes());
                    }
                }
                drop(map);
                // simpledrm and virtio only scan out what was flagged; a driver without the ioctl
                // scans the buffer out as it is.
                let clip = ClipRect::new(0, 0, output.width as u16, output.height as u16);
                let _ = head.card.dirty_framebuffer(output.fb, &[clip]);
            }
        }
    }
}
