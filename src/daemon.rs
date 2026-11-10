use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::canvas::{Canvas, Face};
use crate::jobs::{self, Clock, Row, Snapshot};
use crate::kms::Display;
use crate::scene::{self, STEPS, Scene, Theme};

const PID: &str = "/run/formalboot.pid";
const NEW_ROOT: &str = "/sysroot";
/// How long the last boot took to reach `quit`, which paces the bar on this one.
const DURATION: &str = "/var/lib/formalboot/duration";

const FADE_IN: f32 = 0.3;
const LISTS_OUT: f32 = 0.12;
/// The finished frame stays this long before the exit, so it is scanned out at least once.
const HOLD: f32 = 0.15;

fn expected() -> Option<f32> {
    // The initrd sees the real root's file under /sysroot once that is mounted.
    [DURATION.to_string(), format!("{NEW_ROOT}{DURATION}")]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok()?.trim().parse().ok())
        .filter(|secs: &f32| *secs > 0.0)
}

pub fn run(theme: Theme, shutdown: bool) -> Result<(), String> {
    std::fs::write(PID, std::process::id().to_string()).map_err(|e| format!("{PID}: {e}"))?;
    let quit = Arc::new(AtomicBool::new(false));
    let newroot = Arc::new(AtomicBool::new(false));
    for (signal, flag) in [(libc::SIGTERM, &quit), (libc::SIGINT, &quit), (libc::SIGUSR1, &newroot)] {
        signal_hook::flag::register(signal, flag.clone()).map_err(|e| e.to_string())?;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || crate::systemd::watch(tx, shutdown));

    let start = Instant::now();
    let mut face = Face::new();
    let mut display = Display::default();
    let mut canvases: Vec<Canvas> = Vec::new();
    let mut scanned: Option<Instant> = None;
    let mut lit: Option<Instant> = None;
    let mut snapshot = Snapshot::default();
    let mut rows: Vec<Row> = Vec::new();
    let mut clock = Clock::default();
    let mut pace: Option<f32> = None;
    let mut paced = start;
    let mut quitting: Option<Instant> = None;
    let mut drawn = None;

    loop {
        let now = Instant::now();
        if newroot.swap(false, Ordering::Relaxed) {
            // The initrd's /run, /dev and /proc move under the new root, and systemd deletes the
            // initrd's files once it has switched.
            std::os::unix::fs::chroot(NEW_ROOT)
                .and_then(|()| std::env::set_current_dir("/"))
                .map_err(|e| format!("{NEW_ROOT}: {e}"))?;
        }
        if scanned.is_none_or(|t| now.duration_since(t) >= Duration::from_millis(500)) {
            scanned = Some(now);
            if display.scan() {
                canvases = display.sizes().into_iter().map(|(w, h)| Canvas::new(w, h, theme.backdrop)).collect();
                drawn = None;
            }
            if lit.is_none() && !canvases.is_empty() {
                lit = Some(now);
            }
        }
        while let Ok(next) = rx.try_recv() {
            clock.observe(&next, now);
            rows = jobs::rows(&next);
            snapshot = next;
            drawn = None;
        }
        if pace.is_none() && now.duration_since(paced) >= Duration::from_secs(1) {
            paced = now;
            pace = expected();
        }
        if quitting.is_none() && quit.load(Ordering::Relaxed) {
            quitting = Some(now);
        }

        let since_quit = quitting.map(|t| now.duration_since(t).as_secs_f32());
        let steps = match since_quit {
            Some(_) => STEPS,
            None => ((start.elapsed().as_secs_f32() / pace.unwrap_or(12.0) * STEPS as f32) as u32).min(STEPS - 1),
        };
        let opacity = lit.map_or(0.0, |t| (now.duration_since(t).as_secs_f32() / FADE_IN).min(1.0));
        let lists = since_quit.map_or(1.0, |t| (1.0 - t / LISTS_OUT).max(0.0));
        let tick = if rows.iter().any(|r| r.running) { (start.elapsed().as_millis() / 150) as usize } else { 0 };
        let key = (steps, opacity.to_bits(), lists.to_bits(), tick);
        if drawn != Some(key) && !canvases.is_empty() {
            drawn = Some(key);
            let elapsed: Vec<Option<f32>> = rows.iter().map(|r| clock.elapsed(r.id, now)).collect();
            let slowest: Vec<(String, f32)> = clock.slowest().cloned().collect();
            let scene = Scene {
                shutdown,
                steps,
                rows: &rows,
                elapsed: &elapsed,
                slowest: &slowest,
                failed: &snapshot.failed,
                tick,
                opacity,
                lists,
            };
            for canvas in &mut canvases {
                scene::draw(canvas, &mut face, &theme, &scene);
            }
            display.present(&canvases);
        }
        if since_quit.is_some_and(|t| t >= LISTS_OUT + HOLD) || (quitting.is_some() && canvases.is_empty()) {
            break;
        }
        let fading = opacity < 1.0 || quitting.is_some();
        std::thread::sleep(Duration::from_millis(if fading { 16 } else { 50 }));
    }

    if !shutdown && let Some(dir) = Path::new(DURATION).parent() {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(DURATION, format!("{:.1}\n", start.elapsed().as_secs_f32()));
    }
    let _ = std::fs::remove_file(PID);
    Ok(())
}

/// The daemon's pid, if the pid file still names one.
fn daemon() -> Option<i32> {
    let pid: i32 = std::fs::read_to_string(PID).ok()?.trim().parse().ok()?;
    alive(pid).then_some(pid)
}

// The comm check keeps a stale file's pid, reused by something else, from being signalled.
fn alive(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "formalboot")
}

fn gone_within(pid: i32, limit: Duration) -> bool {
    let start = Instant::now();
    while alive(pid) {
        if start.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    true
}

fn signal(pid: i32, signal: i32) {
    // SAFETY: kill takes no pointers.
    unsafe { libc::kill(pid, signal) };
}

/// Returns once the daemon is gone and its DRM devices are closed, so a compositor started
/// after this finds the card free.
pub fn quit(patience: Option<u64>) -> Result<(), String> {
    let Some(pid) = daemon() else { return Ok(()) };
    if patience.is_some_and(|secs| gone_within(pid, Duration::from_secs(secs))) {
        return Ok(());
    }
    signal(pid, libc::SIGTERM);
    // One that never answers is killed: a compositor held back by a hung splash is no desktop.
    if !gone_within(pid, Duration::from_secs(5)) {
        signal(pid, libc::SIGKILL);
        gone_within(pid, Duration::from_secs(5));
    }
    Ok(())
}

pub fn newroot() -> Result<(), String> {
    let Some(pid) = daemon() else { return Ok(()) };
    signal(pid, libc::SIGUSR1);
    let start = Instant::now();
    while std::fs::read_link(format!("/proc/{pid}/root")).is_ok_and(|root| root != Path::new(NEW_ROOT)) {
        if start.elapsed() >= Duration::from_secs(2) {
            return Err(format!("the daemon did not move to {NEW_ROOT}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}
