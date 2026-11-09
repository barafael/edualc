//! The helper process that decodes images, confined (see the parent module): Linux only, as
//! in the browser demo the page is the sandbox and images are decoded where they are shown.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr, Scope,
};
use rustix::fs::{Mode, OFlags};
use rustix::process::{DumpableBehavior, Resource, Rlimit};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    Decoded, Job, MAX_ANSWER_BYTES, MAX_INPUT_BYTES, MAX_SENT_BYTES, MAX_SHOWN_ANSWER_BYTES,
    MAX_SIDE_PX, Picture, Pixels, Refusal, SENT_LONG_EDGE_PX, SHOWN_SIDE_PX, Shown,
    THUMBNAIL_SIDE_PX, decoded, shown,
};

/// The newest Landlock this helper knows.
const ABI_KNOWN: ABI = ABI::V9;

/// How long a decode may take, and the helper's own processor time.
const PATIENCE: Duration = Duration::from_secs(10);
/// The helper's address space: decoding's allocations and the program around them.
const MAX_ADDRESS_SPACE_BYTES: u64 = 1024 * 1024 * 1024;

/// Why the daemon has no image from the helper.
#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("couldn't start the image decoder: {0}")]
    Start(#[source] std::io::Error),
    #[error("the image decoder has no pipes")]
    NoPipes,
    #[error("couldn't hand the image to the decoder: {0}")]
    Hand(#[source] std::io::Error),
    #[error("couldn't read the image decoder's answer: {0}")]
    Read(#[source] std::io::Error),
    #[error("the image decoder took longer than {PATIENCE:?}")]
    Slow,
    #[error("the image decoder failed ({0})")]
    Failed(std::process::ExitStatus),
    #[error("the image decoder answered more than an answer comes to")]
    TooMuch,
    #[error("the image decoder's answer doesn't parse: {0}")]
    Unreadable(#[source] postcard::Error),
    #[error("the image decoder's answer doesn't hold together")]
    Malformed,
    #[error(transparent)]
    Refused(#[from] Refusal),
}

/// The helper: this program, as it runs.
const HELPER: &str = "/proc/self/exe";

/// Has the helper decode `bytes`, to send.
pub(crate) async fn decode(bytes: &[u8]) -> Result<Picture, Error> {
    let answer = exchange(std::path::Path::new(HELPER), Job::Send, bytes).await?;
    checked(&answer)
}

/// Has the helper decode `bytes`, to show.
pub(crate) async fn show(bytes: &[u8]) -> Result<Pixels, Error> {
    let answer = exchange(std::path::Path::new(HELPER), Job::Show, bytes).await?;
    checked_shown(&answer)
}

/// The helper's whole answer to `job` for `bytes`.
async fn exchange(program: &std::path::Path, job: Job, bytes: &[u8]) -> Result<Vec<u8>, Error> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Refusal::TooManyBytes.into());
    }
    let max_answer_bytes = match job {
        Job::Send => MAX_ANSWER_BYTES,
        Job::Show => MAX_SHOWN_ANSWER_BYTES,
    };
    let mut child = tokio::process::Command::new(program)
        .arg(job.as_ref())
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(Error::Start)?;
    let (Some(mut stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
        return Err(Error::NoPipes);
    };
    let talking = async {
        let handing = async {
            stdin.write_all(bytes).await?;
            // Its end of input: the helper reads to it.
            drop(stdin);
            Ok(())
        };
        let reading = async {
            let mut answer = Vec::new();
            let limit = u64::try_from(max_answer_bytes + 1).unwrap_or(u64::MAX);
            stdout.take(limit).read_to_end(&mut answer).await?;
            Ok(answer)
        };
        let (handed, answer): (std::io::Result<()>, std::io::Result<Vec<u8>>) =
            tokio::join!(handing, reading);
        let status = child.wait().await.map_err(Error::Read)?;
        if !status.success() {
            return Err(Error::Failed(status));
        }
        handed.map_err(Error::Hand)?;
        answer.map_err(Error::Read)
    };
    // Late, it is dropped, and with it the helper, killed.
    let answer = tokio::time::timeout(PATIENCE, talking)
        .await
        .map_err(|_elapsed| Error::Slow)??;
    if answer.len() > max_answer_bytes {
        return Err(Error::TooMuch);
    }
    Ok(answer)
}

/// The helper's answer, taken only if it is one whole answer that holds together.
fn checked(answer: &[u8]) -> Result<Picture, Error> {
    if answer.len() > MAX_ANSWER_BYTES {
        return Err(Error::TooMuch);
    }
    let (decoded, rest) =
        postcard::take_from_bytes::<Decoded>(answer).map_err(Error::Unreadable)?;
    if !rest.is_empty() {
        return Err(Error::Malformed);
    }
    let picture = match decoded {
        Decoded::Image(picture) => picture,
        Decoded::Refused(refusal) => return Err(refusal.into()),
    };
    let thumbnail = &picture.thumbnail;
    let fits = picture.width_px <= SENT_LONG_EDGE_PX
        && picture.height_px <= SENT_LONG_EDGE_PX
        && thumbnail.width_px <= THUMBNAIL_SIDE_PX
        && thumbnail.height_px <= THUMBNAIL_SIDE_PX
        && !picture.bytes.is_empty()
        && picture.bytes.len() <= MAX_SENT_BYTES;
    if thumbnail.whole() && fits {
        Ok(picture)
    } else {
        Err(Error::Malformed)
    }
}

/// The helper's answer to show, taken as [`checked`] takes one to send.
fn checked_shown(answer: &[u8]) -> Result<Pixels, Error> {
    if answer.len() > MAX_SHOWN_ANSWER_BYTES {
        return Err(Error::TooMuch);
    }
    let (shown, rest) = postcard::take_from_bytes::<Shown>(answer).map_err(Error::Unreadable)?;
    if !rest.is_empty() {
        return Err(Error::Malformed);
    }
    let pixels = match shown {
        Shown::Image(pixels) => pixels,
        Shown::Refused(refusal) => return Err(refusal.into()),
    };
    let fits = (1..=SHOWN_SIDE_PX).contains(&pixels.width_px)
        && (1..=SHOWN_SIDE_PX).contains(&pixels.height_px);
    if pixels.whole() && fits {
        Ok(pixels)
    } else {
        Err(Error::Malformed)
    }
}

/// The helper: confined, then the bytes on its standard input made into what `job` asks, and
/// one answer on its standard output.
pub(crate) fn run(job: Job) -> ExitCode {
    if let Err(error) = confine() {
        // Nothing is read unconfined; the daemon sees the failure (stderr is for a person
        // running it by hand).
        eprintln!("edualc {}: couldn't confine itself: {error}", job.as_ref());
        return ExitCode::FAILURE;
    }
    let mut bytes = Vec::new();
    let limit = u64::try_from(MAX_INPUT_BYTES + 1).unwrap_or(u64::MAX);
    if std::io::stdin()
        .lock()
        .take(limit)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return ExitCode::FAILURE;
    }
    let refused = (bytes.len() > MAX_INPUT_BYTES).then_some(Refusal::TooManyBytes);
    let encoded = match job {
        Job::Send => postcard::to_stdvec(&match refused
            .map_or_else(|| decoded(&bytes, MAX_SIDE_PX), Err)
        {
            Ok(picture) => Decoded::Image(picture),
            Err(refusal) => Decoded::Refused(refusal),
        }),
        Job::Show => postcard::to_stdvec(&match refused.map_or_else(|| shown(&bytes), Err) {
            Ok(pixels) => Shown::Image(pixels),
            Err(refusal) => Shown::Refused(refusal),
        }),
    };
    let Ok(encoded) = encoded else {
        return ExitCode::FAILURE;
    };
    let mut stdout = std::io::stdout().lock();
    if stdout
        .write_all(&encoded)
        .and_then(|()| stdout.flush())
        .is_err()
    {
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

#[derive(Debug, thiserror::Error)]
enum ConfineError {
    #[error("{0}")]
    System(#[from] rustix::io::Errno),
    #[error("descriptor {fd} is open ({target})")]
    Inherited { fd: i32, target: String },
    #[error("{0}")]
    Landlock(#[from] landlock::RulesetError),
    #[error("{0}")]
    Seccomp(#[from] seccompiler::Error),
    #[error("{0}")]
    SeccompBackend(#[from] seccompiler::BackendError),
}

/// Confines the helper, in an order where nothing set up later needs what was taken earlier:
/// closed to other processes, sure to hold nothing but its standard streams, then no file,
/// socket or signal (Landlock), bounded (rlimits), and last the system calls decoding makes,
/// with no memory made executable (seccomp).
fn confine() -> Result<(), ConfineError> {
    rustix::process::set_dumpable_behavior(DumpableBehavior::NotDumpable)?;
    only_standard_streams()?;
    Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(ABI_KNOWN))?
        .handle_access(AccessNet::BindTcp | AccessNet::ConnectTcp)?
        .scope(Scope::AbstractUnixSocket | Scope::Signal)?
        .create()?
        .restrict_self()?;
    let seconds = PATIENCE.as_secs();
    for (resource, limit) in [
        (Resource::As, MAX_ADDRESS_SPACE_BYTES),
        (Resource::Cpu, seconds),
        (Resource::Fsize, 0),
        (Resource::Core, 0),
        (Resource::Nofile, 3),
    ] {
        rustix::process::setrlimit(
            resource,
            Rlimit {
                current: Some(limit),
                maximum: Some(limit),
            },
        )?;
    }
    seccompiler::apply_filter(&filter()?)?;
    Ok(())
}

/// Refuses to go on with any descriptor but the standard streams: one inherited by mistake
/// could reach what the rest of the confinement keeps away.
fn only_standard_streams() -> Result<(), ConfineError> {
    let listing = rustix::fs::open(
        "/proc/self/fd",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let own = listing.as_raw_fd();
    // Owned by the listing, which opens no descriptor of its own.
    for entry in rustix::fs::Dir::new(listing)? {
        let entry = entry?;
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if fd > 2 && fd != own {
            let target = std::fs::read_link(format!("/proc/self/fd/{fd}"))
                .map_or_else(|error| error.to_string(), |path| path.display().to_string());
            return Err(ConfineError::Inherited { fd, target });
        }
    }
    Ok(())
}

/// The system calls the helper makes once confined: reading its input, writing its answer,
/// memory (never executable), and ending. Any other kills it.
fn filter() -> Result<BpfProgram, ConfineError> {
    let on_fd = |fd: u64| -> Result<SeccompRule, ConfineError> {
        Ok(SeccompRule::new(vec![SeccompCondition::new(
            0,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Eq,
            fd,
        )?])?)
    };
    let not_executable = || -> Result<SeccompRule, ConfineError> {
        let exec = u64::from(libc::PROT_EXEC.unsigned_abs());
        Ok(SeccompRule::new(vec![SeccompCondition::new(
            2,
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::MaskedEq(exec),
            0,
        )?])?)
    };
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    rules.insert(libc::SYS_read, vec![on_fd(0)?]);
    rules.insert(libc::SYS_write, vec![on_fd(1)?, on_fd(2)?]);
    rules.insert(libc::SYS_mmap, vec![not_executable()?]);
    rules.insert(libc::SYS_mprotect, vec![not_executable()?]);
    for always in [
        libc::SYS_munmap,
        libc::SYS_mremap,
        libc::SYS_brk,
        libc::SYS_madvise,
        libc::SYS_futex,
        libc::SYS_rt_sigreturn,
        libc::SYS_rt_sigprocmask,
        libc::SYS_sigaltstack,
        libc::SYS_getrandom,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ] {
        rules.insert(always, Vec::new());
    }
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::KillProcess,
        SeccompAction::Allow,
        std::env::consts::ARCH.try_into()?,
    )?;
    Ok(filter.try_into()?)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use image::{DynamicImage, ImageFormat};

    use super::*;
    use crate::decode::{SENT_VISUAL_TOKENS, sent_size};
    use crate::external::wire::MediaType;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let channel = |coordinate: u32| u8::try_from(coordinate % 256).unwrap_or(0);
        let image = image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([channel(x), channel(y), 128, 255])
        });
        let mut bytes = Cursor::new(Vec::new());
        image
            .write_to(&mut bytes, ImageFormat::Png)
            .expect("encodes");
        bytes.into_inner()
    }

    /// The sizes in Anthropic's vision docs' table, high-resolution tier: 1920x1080 and
    /// 2000x1500 are "Not resized", 3840x2160 is "2576x1449 px", 4,784 tokens.
    #[test]
    fn images_are_scaled_as_the_vision_docs_say() {
        assert_eq!(sent_size(1920, 1080), (1920, 1080));
        assert_eq!(sent_size(2000, 1500), (2000, 1500));
        assert_eq!(sent_size(3840, 2160), (2576, 1449));
        assert_eq!(sent_size(200, 200), (200, 200));
        // Within the long edge, but more tokens than taken: scaled to fit those.
        let (width, height) = sent_size(2400, 2400);
        assert!(width.div_ceil(28) * height.div_ceil(28) <= SENT_VISUAL_TOKENS);
        assert_eq!(width, height);
    }

    #[test]
    fn a_png_comes_back_scaled_with_its_thumbnail() {
        let picture = decoded(&png(3000, 1000), MAX_SIDE_PX).expect("decodes");
        assert_eq!(picture.media_type, MediaType::Png);
        assert_eq!((picture.width_px, picture.height_px), (2576, 858));
        assert_eq!(picture.thumbnail.width_px, THUMBNAIL_SIDE_PX);
        assert_eq!(
            picture.thumbnail.rgba.len(),
            usize::try_from(picture.thumbnail.width_px * picture.thumbnail.height_px * 4)
                .expect("fits")
        );
        assert!(checked(&postcard::to_stdvec(&Decoded::Image(picture)).expect("encodes")).is_ok());
    }

    /// An image to show is scaled down to fit, never up; its answer is taken only whole and
    /// within that size.
    #[test]
    fn an_image_to_show_fits_its_room_and_its_answer_is_checked() {
        let large = shown(&png(3000, 1000)).expect("decodes");
        assert_eq!((large.width_px, large.height_px), (SHOWN_SIDE_PX, 320));
        assert!(large.whole());
        let small = shown(&png(40, 20)).expect("decodes");
        assert_eq!((small.width_px, small.height_px), (40, 20));
        assert_eq!(small.rgba.get(..4), Some([0, 0, 128, 255].as_slice()));
        let answer = |shown: &Shown| postcard::to_stdvec(shown).expect("encodes");
        assert_eq!(
            checked_shown(&answer(&Shown::Image(small.clone()))).expect("taken"),
            small
        );
        assert!(matches!(
            checked_shown(&answer(&Shown::Refused(Refusal::UnknownFormat))),
            Err(Error::Refused(Refusal::UnknownFormat))
        ));
        let mut short = small;
        short.rgba.pop();
        let wide = Pixels {
            width_px: SHOWN_SIDE_PX + 1,
            height_px: 1,
            rgba: vec![0; 4 * (SHOWN_SIDE_PX as usize + 1)],
        };
        let empty = Pixels {
            width_px: 0,
            height_px: 0,
            rgba: Vec::new(),
        };
        for broken in [short, wide, empty] {
            assert!(matches!(
                checked_shown(&answer(&Shown::Image(broken))),
                Err(Error::Malformed)
            ));
        }
        assert!(matches!(
            checked_shown(&vec![0; MAX_SHOWN_ANSWER_BYTES + 1]),
            Err(Error::TooMuch)
        ));
    }

    /// A GIF and a WebP are read like a PNG, and go as one, their pixels kept.
    #[test]
    fn a_gif_or_webp_is_read_and_goes_as_a_png() {
        let source = image::RgbaImage::from_fn(30, 20, |x, y| {
            image::Rgba([
                u8::try_from(x * 8).unwrap_or(0),
                u8::try_from(y * 12).unwrap_or(0),
                64,
                255,
            ])
        });
        for format in [ImageFormat::Gif, ImageFormat::WebP] {
            let mut bytes = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(source.clone())
                .write_to(&mut bytes, format)
                .expect("encodes");
            let bytes = bytes.into_inner();
            let picture = decoded(&bytes, MAX_SIDE_PX).expect("decodes");
            assert_eq!(picture.media_type, MediaType::Png, "{format:?}");
            assert_eq!((picture.width_px, picture.height_px), (30, 20));
            let shown = shown(&bytes).expect("decodes");
            assert_eq!((shown.width_px, shown.height_px), (30, 20));
        }
        // The WebP is lossless: the pixels sent are the pixels it held.
        let mut webp = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(source.clone())
            .write_to(&mut webp, ImageFormat::WebP)
            .expect("encodes");
        let shown = shown(webp.get_ref()).expect("decodes");
        assert_eq!(shown.rgba, source.into_raw());
    }

    #[test]
    fn what_isnt_an_image_or_is_too_large_is_refused() {
        assert_eq!(
            decoded(b"BM a bitmap's start", MAX_SIDE_PX),
            Err(Refusal::UnknownFormat)
        );
        assert_eq!(
            decoded(b"plain text", MAX_SIDE_PX),
            Err(Refusal::UnknownFormat)
        );
        assert_eq!(decoded(&png(300, 100), 200), Err(Refusal::TooLarge));
        let cut = png(10, 10);
        let cut = cut.get(..40).expect("longer than 40 bytes");
        assert!(matches!(
            decoded(cut, MAX_SIDE_PX),
            Err(Refusal::Undecodable(_))
        ));
    }

    /// The confinement holds: confined as the helper is, a process reaching for a file is killed
    /// (the real filter, whose answer to anything not allowed is death). The test starts itself
    /// again as that process.
    #[test]
    fn a_confined_helper_is_killed_reaching_for_a_file() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "EDUALC_CONFINED_CHILD";
        if std::env::var_os(CHILD).is_some() {
            confine().expect("confined");
            let opened = std::fs::File::open("/etc/hostname");
            // Still here: the parent sees an exit, not the kill it waits for.
            std::process::exit(if opened.is_ok() { 3 } else { 4 });
        }
        let status = std::process::Command::new(std::env::current_exe().expect("this test"))
            .args([
                "--exact",
                "decode::helper::tests::a_confined_helper_is_killed_reaching_for_a_file",
                "--test-threads=1",
            ])
            .env(CHILD, "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("ran");
        assert_eq!(status.signal(), Some(libc::SIGSYS), "{status}");
    }

    /// The helper's answer is taken as untrusted: one whole answer, sizes that agree.
    #[test]
    fn an_answer_that_doesnt_hold_together_is_refused() {
        let picture = decoded(&png(40, 20), MAX_SIDE_PX).expect("decodes");
        let mut short = picture.clone();
        short.thumbnail.rgba.pop();
        let mut wide = picture.clone();
        wide.width_px = SENT_LONG_EDGE_PX + 1;
        for broken in [short, wide] {
            let answer = postcard::to_stdvec(&Decoded::Image(broken)).expect("encodes");
            assert!(matches!(checked(&answer), Err(Error::Malformed)));
        }
        let mut trailing = postcard::to_stdvec(&Decoded::Image(picture)).expect("encodes");
        trailing.push(0);
        assert!(matches!(checked(&trailing), Err(Error::Malformed)));
        assert!(matches!(checked(&[7, 7, 7]), Err(Error::Unreadable(_))));
    }
}
