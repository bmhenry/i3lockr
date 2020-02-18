use std::borrow::Cow;
use std::error::Error;
use std::hint::unreachable_unchecked;
use std::io::ErrorKind::WouldBlock;
use std::io::{self, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use std::os::unix::process::ExitStatusExt;
use std::thread::sleep;

use imgref::ImgRefMut;

use rgb::alt::BGRA8;
use rgb::{ComponentBytes, FromSlice};

use scrap::{Capturer, Display, Frame};

use structopt::clap::Format;
use structopt::StructOpt;

use xcb::Connection;

mod cli;
mod macros;

use cli::Cli;

#[cfg(any(feature = "png", feature = "jpeg"))]
mod algorithms;
#[cfg(any(feature = "png", feature = "jpeg"))]
use imagefmt::ColFmt;
#[cfg(any(feature = "png", feature = "jpeg"))]
use xcb::randr;

#[cfg(feature = "scale")]
mod scale;
#[cfg(feature = "scale")]
use scale::Scale;

#[cfg(feature = "blur")]
mod blur;
#[cfg(feature = "blur")]
use blur::Blur;

#[cfg(feature = "brightness")]
mod brightness;
#[cfg(feature = "brightness")]
use brightness::BrightnessAdj;

fn main() -> Result<(), Box<dyn Error>> {
    timer_start!(everything);
    // parse args, handle custom `--version`
    let args = Cli::from_args();
    if args.version {
        eprintln!(
            "{} v{} compiled for '{}' at {} ({}@{})",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            env!("TARGET"),
            env!("TIME"),
            env!("GIT_BRANCH"),
            env!("GIT_COMMIT")
        );
        return Ok(());
    }

    // init debug macro
    macro_rules! debug {
        ($($arg:tt)*) => {
            if cfg!(debug_assertions) || args.verbose {
                eprintln!("{f}:{l}:{c} {fmt}", f=file!(), l=line!(), c=column!(), fmt=format!($($arg)*));
            }
        }
    }

    debug!("Found args: {:#?}", args);

    let (conn, screen_num) = Connection::connect(None)?;

    // setup scrap
    timer_start!(scrap);
    let disp = Display::primary()?;
    let mut capture = Capturer::new(disp)?;
    let (w, h) = (capture.width(), capture.height());
    timer_time!("Setting up scrap", scrap);

    // take the screenshot
    timer_start!(screenshot);
    let mut buffer: Frame;
    loop {
        match capture.frame() {
            Ok(buf) => {
                buffer = buf;
                break;
            }
            Err(e) => {
                if e.kind() == WouldBlock {
                    sleep(Duration::from_millis(33));
                    continue;
                } else {
                    return Err(e.into());
                }
            }
        }
    }
    timer_time!("Capturing screenshot", screenshot);

    // convert to imgref
    timer_start!(convert);
    let buf_bgra = buffer.as_bgra_mut();
    let mut screenshot = ImgRefMut::new(buf_bgra, w, h);
    timer_time!("Converting image", convert);

    // scale down
    let mut scaled_img: Option<ImgRefMut<BGRA8>> = None;
    if let Some(f) = args.factor {
        #[cfg(feature = "scale")]
        {
            timer_start!(downscale);
            unsafe { scaled_img = Some(screenshot.scale_down(f)) };
            timer_time!("Downscaling", downscale);
        }
        #[cfg(not(feature = "scale"))]
        warn_disabled!("scale");
    }

    // blur
    if let Some(r) = args.radius {
        #[cfg(feature = "blur")]
        {
            timer_start!(blur);
            unsafe { screenshot.blur(r)? };
            timer_time!("Blurring", blur);
        }
        #[cfg(not(feature = "blur"))]
        warn_disabled!("blur");
    }

    // scale back up
    if let Some(f) = args.factor {
        #[cfg(feature = "scale")]
        {
            timer_start!(upscale);
            unsafe { screenshot.scale_up(f) };
            timer_time!("Upscaling", upscale);
        }
        #[cfg(not(feature = "scale"))]
        warn_disabled!("scale");
    }

    // brighten
    if let Some(b) = args.bright {
        #[cfg(feature = "brightness")]
        {
            timer_start!(bright);
            screenshot.brighten(b);
            timer_time!("Brightening", bright);
        }
        #[cfg(not(feature = "brightness"))]
        warn_disabled!("brightness");
    }

    // darken
    if let Some(d) = args.dark {
        #[cfg(feature = "brightness")]
        {
            timer_start!(dark);
            screenshot.darken(d);
            timer_time!("Darkening", dark);
        }
        #[cfg(not(feature = "brightness"))]
        warn_disabled!("brightness");
    }

    // overlay/invert on each monitor
    if let Some(ref path) = args.path {
        #[cfg(any(feature = "png", feature = "jpeg"))]
        {
            timer_start!(decode);
            let image = imagefmt::read(path, ColFmt::BGRA)?;
            timer_time!("Decoding overlay image", decode);

            // get handle on monitors
            let screen = conn
                .get_setup()
                .roots()
                .nth(screen_num as usize)
                .unwrap_or_else(|| unreachable!());

            let cookie = randr::get_screen_resources(&conn, screen.root());
            let reply = cookie.get_reply()?;

            for (w, h, x, y) in reply
                .crtcs()
                .iter()
                .filter_map(|crtc| {
                    randr::get_crtc_info(&conn, *crtc, reply.timestamp())
                        .get_reply()
                        .ok()
                })
                .enumerate()
                .filter(|(i, m)| m.mode() != 0 && !args.ignore.contains(i))
                .map(|(_, m)| {
                    (
                        usize::from(m.width()),
                        usize::from(m.height()),
                        m.x() as usize,
                        m.y() as usize,
                    )
                })
            {
                let (x_off, y_off) = if args.pos.is_empty() {
                    if image.w > w || image.h > h {
                        eprintln!(
                            "{}",
                            Format::Warning(
                                "Your image is larger than your monitor, image positions may be off!"
                                )
                            );
                    }
                    (w / 2 - image.w / 2 + x, h / 2 - image.h / 2 + y)
                } else {
                    unsafe {
                        (
                            wrap_to_screen(*args.pos.get_unchecked(0), w + x),
                            wrap_to_screen(*args.pos.get_unchecked(1), h + y),
                        )
                    }
                };

                debug!(
                    "Calculated image position on monitor: ({},{})",
                    x_off, y_off
                );

                timer_start!(overlay);
                algorithms::overlay(&mut shot, &image, x_off, y_off, args.invert);
                timer_time!("Overlaying image", overlay);
            }
        }
        #[cfg(not(any(feature = "png", feature = "jpeg")))]
        warn_disabled!("png/jpeg overlay");
    }

    //TODO draw text

    // check if we're forking
    timer_start!(fork);
    let nofork = forking(args.i3lock.iter().map(|x| x.as_os_str().to_string_lossy()));
    timer_time!("Checking for nofork", fork);

    // call i3lock
    debug!("Calling i3lock with args: {:?}", args.i3lock);
    let mut cmd = Command::new("i3lock")
        .args(&[
            "-i",
            "/dev/stdin",
            //FIXME
            &format!("--raw={}x{}:native", w, h),
        ])
        .args(args.i3lock)
        .stdin(Stdio::piped())
        .spawn()?;

    // pass image bytes
    cmd.stdin
        .as_mut()
        .expect("Failed to take cmd.stdin.as_mut()")
        .write_all(screenshot.into_buf().as_bytes())?;

    timer_time!("Everything", everything);

    if nofork {
        debug!("Asked i3lock not to fork, calling wait()");
        match cmd.wait() {
            Ok(status) => status_to_result(status),
            Err(e) => Err(e.into()),
        }
    } else {
        match cmd.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(status)) => status_to_result(status),
            Err(e) => Err(e.into()),
        }
    }
}

fn status_to_result(status: ExitStatus) -> Result<(), Box<dyn Error>> {
    if status.success() {
        Ok(())
    } else if let Some(code) = status.code() {
        Err(io::Error::from_raw_os_error(code).into())
    } else {
        Err(format!(
            "Killed by signal: {}",
            status
                .signal()
                .unwrap_or_else(|| unsafe { unreachable_unchecked() })
        )
        .into())
    }
}

// credit: @williewillus#8490
#[cfg(any(feature = "png", feature = "jpeg"))]
fn wrap_to_screen(idx: isize, len: usize) -> usize {
    if idx.is_negative() {
        let pos = -idx as usize % len;
        if pos == 0 {
            0
        } else {
            len - pos
        }
    } else {
        idx as usize % len
    }
}

fn forking<'a, I>(args: I) -> bool
where
    I: Iterator<Item = Cow<'a, str>> + Clone,
{
    args.clone().any(|x| x == "--nofork")
        || args
            .filter(|x| !x.starts_with("--"))
            .any(|x| x.contains('n'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nofork() {
        assert!(forking(
            [
                "-n",
                "--insidecolor=542095ff",
                "--ringcolor=ffffffff",
                "--line-uses-inside"
            ]
            .iter()
            .map(|x| Cow::Borrowed(*x))
        ));
        assert!(!forking(
            [
                "--insidecolor=542095ff",
                "--ringcolor=ffffffff",
                "--line-uses-inside"
            ]
            .iter()
            .map(|x| Cow::Borrowed(*x))
        ));
        assert!(forking(
            [
                "--insidecolor=542095ff",
                "--ringcolor=ffffffff",
                "-en",
                "--line-uses-inside"
            ]
            .iter()
            .map(|x| Cow::Borrowed(*x))
        ));
        assert!(!forking(
            [
                "--ringcolor=ffffffff",
                "-e",
                "--insidecolor=542095ff",
                "--line-uses-inside"
            ]
            .iter()
            .map(|x| Cow::Borrowed(*x))
        ));
    }
}
