mod canvas;
#[cfg(target_os = "linux")]
mod daemon;
mod jobs;
#[cfg(target_os = "linux")]
mod kms;
mod scene;
#[cfg(target_os = "linux")]
mod systemd;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use canvas::{Canvas, Face};
use jobs::{Failure, Job, Snapshot};
use scene::{BLOCK, STEPS, Scene, Theme};

const USAGE: &str = "\
formalboot [--shutdown] [--theme <dir>]     draw the boot (or shutdown) screen until told to quit
formalboot quit [--patience <seconds>]      fill the bar, leave the finished frame up and exit;
                                            with --patience, first give it that long to go by itself
formalboot newroot                          follow the boot into the root the initrd mounted on /sysroot
formalboot frame --out <png> [--theme <dir>] (--scale <1-4> | --demo <width>x<height>)
                                            the finished frame on its own canvas, or a sample boot";

const THEME: &str = "/usr/local/share/formalos-boot";

struct Args(Vec<String>);

impl Args {
    fn flag(&mut self, name: &str) -> bool {
        let at = self.0.iter().position(|a| a == name);
        at.map(|i| self.0.remove(i)).is_some()
    }

    fn value(&mut self, name: &str) -> Result<Option<String>, String> {
        let Some(i) = self.0.iter().position(|a| a == name) else { return Ok(None) };
        if i + 1 >= self.0.len() {
            return Err(format!("{name} needs a value"));
        }
        self.0.remove(i);
        Ok(Some(self.0.remove(i)))
    }

    fn done(self) -> Result<(), String> {
        if self.0.is_empty() { Ok(()) } else { Err(format!("unexpected argument {}\n{USAGE}", self.0[0])) }
    }
}

fn write_png(canvas: &Canvas, path: &Path) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), canvas.w as u32, canvas.h as u32);
    encoder.set_color(png::ColorType::Rgb);
    let rgb: Vec<u8> = canvas.px.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
    let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(&rgb).map_err(|e| e.to_string())
}

fn demo() -> (Snapshot, Vec<(String, f32)>) {
    let units = [
        (1, "graphical.target", false),
        (2, "multi-user.target", false),
        (3, "NetworkManager-wait-online.service", true),
        (4, "network-online.target", false),
        (5, "bluetooth.service", true),
        (6, "greetd.service", false),
        (7, "systemd-user-sessions.service", true),
        (8, "dev-disk-by\\x2duuid-04b474c9\\x2d4caf\\x2d4bd4\\x2d969e\\x2d233bba3134c6.device", true),
        (9, "boot-efi.mount", false),
        (10, "local-fs.target", false),
    ];
    let jobs = units.iter().map(|&(id, unit, running)| Job { id, unit: unit.into(), running }).collect();
    let waits = [(1, vec![2, 6]), (2, vec![4, 5]), (4, vec![3]), (6, vec![7]), (10, vec![9]), (9, vec![8])];
    let failed = vec![Failure {
        unit: "fwupd-refresh.service".into(),
        lines: vec!["Failed to connect to daemon: timed out".into(), "Main process exited, status=1/FAILURE".into()],
    }];
    let slowest = [
        ("systemd-udev-settle", 2.9),
        ("dracut-initqueue", 1.7),
        ("systemd-vconsole-setup", 0.9),
        ("kmod-static-nodes", 0.4),
    ];
    (
        Snapshot { jobs, waits: waits.into_iter().collect(), failed },
        slowest.into_iter().map(|(u, s)| (u.to_string(), s)).collect(),
    )
}

fn frame(mut args: Args) -> Result<(), String> {
    let theme = Theme::load(&PathBuf::from(args.value("--theme")?.unwrap_or(THEME.into())))?;
    let out = PathBuf::from(args.value("--out")?.ok_or("frame needs --out")?);
    let scale = args.value("--scale")?;
    let size = args.value("--demo")?;
    args.done()?;
    let mut face = Face::new();
    let canvas = match (scale, size) {
        (Some(scale), None) => {
            let s: usize = scale.parse().ok().filter(|s| (1..=4).contains(s)).ok_or("--scale is 1 to 4")?;
            // 500 layout pixels a scale step, so the canvas draws at the scale it was asked for.
            let mut full = Canvas::new(BLOCK * s, 500 * s, theme.backdrop);
            let done = Scene {
                shutdown: false,
                steps: STEPS,
                rows: &[],
                pending: 0,
                elapsed: &[],
                slowest: &[],
                failed: &[],
                tick: 0,
                opacity: 1.0,
                lists: 0.0,
            };
            scene::draw(&mut full, &mut face, &theme, &done);
            let skip = (500 - BLOCK) * s / 2;
            let px = full.px[skip * full.w..(skip + BLOCK * s) * full.w].to_vec();
            Canvas { w: BLOCK * s, h: BLOCK * s, px }
        }
        (None, Some(size)) => {
            let (w, h) = size.split_once('x').ok_or("--demo is <width>x<height>")?;
            let (w, h) = (w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?);
            let (snap, slowest) = demo();
            let rows = jobs::rows(&snap);
            let elapsed: Vec<Option<f32>> = rows.iter().map(|r| r.running.then_some(r.id as f32 * 0.7)).collect();
            let mut canvas = Canvas::new(w, h, theme.backdrop);
            let sample = Scene {
                shutdown: false,
                steps: 7,
                rows: &rows,
                pending: snap.jobs.len(),
                elapsed: &elapsed,
                slowest: &slowest,
                failed: &snap.failed,
                tick: 1,
                opacity: 1.0,
                lists: 1.0,
            };
            scene::draw(&mut canvas, &mut face, &theme, &sample);
            canvas
        }
        _ => return Err("frame takes one of --scale and --demo".into()),
    };
    write_png(&canvas, &out)
}

fn run() -> Result<(), String> {
    let mut args = Args(std::env::args().skip(1).collect());
    if args.flag("--help") || args.flag("-h") {
        println!("{USAGE}");
        return Ok(());
    }
    match args.0.first().map(String::as_str) {
        Some("frame") => {
            args.0.remove(0);
            frame(args)
        }
        #[cfg(target_os = "linux")]
        Some("quit") => {
            args.0.remove(0);
            let patience =
                args.value("--patience")?.map(|p| p.parse::<u64>().map_err(|_| "--patience is whole seconds"));
            args.done()?;
            daemon::quit(patience.transpose()?)
        }
        #[cfg(target_os = "linux")]
        Some("newroot") => {
            args.0.remove(0);
            args.done()?;
            daemon::newroot()
        }
        #[cfg(target_os = "linux")]
        _ => {
            let shutdown = args.flag("--shutdown");
            let theme = Theme::load(&PathBuf::from(args.value("--theme")?.unwrap_or(THEME.into())))?;
            args.done()?;
            daemon::run(theme, shutdown)
        }
        #[cfg(not(target_os = "linux"))]
        _ => Err(format!("only `frame` runs off Linux\n{USAGE}")),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("formalboot: {e}");
            ExitCode::FAILURE
        }
    }
}
