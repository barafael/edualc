//! Images from outside (the clipboard; later, transcripts) made safe to show and send. They are
//! decoded in a process of their own, `edualc --decode-image`, which confines itself before it
//! reads a byte: it reaches no file, socket or other process, runs no new code, and has bounded
//! memory and time. It answers with the image encoded again (its metadata dropped, scaled to
//! what Claude takes) and a thumbnail. A crafted image can take over that process, which can do
//! no more than answer.
//!
//! The daemon starts it, hands it the bytes on its standard input and takes one postcard answer
//! from its standard output, read to its end (no framing: the answer is all there is). The
//! answer is untrusted like the image: bounded, parsed as its type, and checked.

use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::os::fd::AsRawFd;
use std::process::{ExitCode, Stdio};
use std::time::Duration;

use image::imageops::FilterType;
use image::{DynamicImage, ImageFormat, ImageReader, Limits};
use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, Ruleset, RulesetAttr, Scope,
};
use rustix::fs::{Mode, OFlags};
use rustix::process::{DumpableBehavior, Resource, Rlimit};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
    SeccompRule,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::external::wire::MediaType;

/// The newest Landlock this helper knows.
const ABI_KNOWN: ABI = ABI::V9;

/// The most bytes taken to decode.
pub(crate) const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
/// The longest side decoded: Anthropic's vision docs, "The maximum dimensions for each image
/// are 8000x8000 px". Larger is refused from its header, before its pixels are allocated.
const MAX_SIDE_PX: u32 = 8000;
/// The most memory decoding allocates.
const MAX_DECODE_BYTES: u64 = 512 * 1024 * 1024;
/// What the current models take at most; larger is scaled down to fit both. Anthropic's vision
/// docs: "High-resolution | Claude 4.7 and later models | 2,576 px | 4,784" (long edge, visual
/// tokens), "Images larger than either limit are downscaled before processing", and an image
/// "costs `⌈width / 28⌉ × ⌈height / 28⌉` visual tokens".
const SENT_LONG_EDGE_PX: u32 = 2576;
const SENT_VISUAL_TOKENS: u32 = 4784;
const VISUAL_TOKEN_PX: u32 = 28;
/// The most an image sent may weigh: base64 makes 4 of every 3 bytes, and Anthropic's vision
/// docs say "5 MB (base64-encoded) on Amazon Bedrock" (10 MB on the Claude API).
const MAX_SENT_BYTES: usize = 5 * 1000 * 1000 / 4 * 3;
/// The longest side of the thumbnail shown in the composer.
pub(crate) const THUMBNAIL_SIDE_PX: u32 = 192;
/// The most the helper may answer, and so the most a pasted image comes to in the window.
pub(crate) const MAX_ANSWER_BYTES: usize = MAX_SENT_BYTES + 1024 * 1024;
/// How long a decode may take, and the helper's own processor time.
const PATIENCE: Duration = Duration::from_secs(10);
/// The helper's address space: decoding's allocations and the program around them.
const MAX_ADDRESS_SPACE_BYTES: u64 = 1024 * 1024 * 1024;

/// What the helper answers.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) enum Decoded {
    Image(Picture),
    Refused(Refusal),
}

/// An image to send, and its thumbnail to show.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Picture {
    pub(crate) media_type: MediaType,
    pub(crate) bytes: Vec<u8>,
    pub(crate) width_px: u32,
    pub(crate) height_px: u32,
    pub(crate) thumbnail: Thumbnail,
}

/// Pixels, four bytes each (red, green, blue, alpha), row by row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Thumbnail {
    pub(crate) width_px: u32,
    pub(crate) height_px: u32,
    pub(crate) rgba: Vec<u8>,
}

/// Why the helper wouldn't make an image of what it was given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub(crate) enum Refusal {
    #[error("more than {MAX_INPUT_BYTES} bytes")]
    TooManyBytes,
    #[error("not a PNG or JPEG image")]
    NotPngOrJpeg,
    #[error("larger than {MAX_SIDE_PX}×{MAX_SIDE_PX} pixels, or more than decoding may take")]
    TooLarge,
    #[error("the image doesn't decode: {0}")]
    Undecodable(String),
    #[error("couldn't encode the image again: {0}")]
    Unencodable(String),
    #[error("more than {MAX_SENT_BYTES} bytes, even as a JPEG")]
    TooHeavy,
}

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
    #[error("the image decoder answered more than {MAX_ANSWER_BYTES} bytes")]
    TooMuch,
    #[error("the image decoder's answer doesn't parse: {0}")]
    Unreadable(#[source] postcard::Error),
    #[error("the image decoder's answer doesn't hold together")]
    Malformed,
    #[error(transparent)]
    Refused(#[from] Refusal),
}

/// Has the helper (this program, as it runs) decode `bytes`.
pub(crate) async fn decode(bytes: &[u8]) -> Result<Picture, Error> {
    decode_with(std::path::Path::new("/proc/self/exe"), bytes).await
}

async fn decode_with(program: &std::path::Path, bytes: &[u8]) -> Result<Picture, Error> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Refusal::TooManyBytes.into());
    }
    let mut child = tokio::process::Command::new(program)
        .arg("--decode-image")
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
    let exchange = async {
        let handing = async {
            stdin.write_all(bytes).await?;
            // Its end of input: the helper reads to it.
            drop(stdin);
            Ok(())
        };
        let reading = async {
            let mut answer = Vec::new();
            let limit = u64::try_from(MAX_ANSWER_BYTES + 1).unwrap_or(u64::MAX);
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
    let answer = tokio::time::timeout(PATIENCE, exchange)
        .await
        .map_err(|_elapsed| Error::Slow)??;
    checked(&answer)
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
    let pixels = u64::from(thumbnail.width_px) * u64::from(thumbnail.height_px);
    let whole = u64::try_from(thumbnail.rgba.len()).ok() == Some(pixels * 4);
    let fits = picture.width_px <= SENT_LONG_EDGE_PX
        && picture.height_px <= SENT_LONG_EDGE_PX
        && thumbnail.width_px <= THUMBNAIL_SIDE_PX
        && thumbnail.height_px <= THUMBNAIL_SIDE_PX
        && !picture.bytes.is_empty()
        && picture.bytes.len() <= MAX_SENT_BYTES;
    if whole && fits {
        Ok(picture)
    } else {
        Err(Error::Malformed)
    }
}

/// The helper: confined, then the bytes on its standard input decoded, and one answer on its
/// standard output.
pub(crate) fn run() -> ExitCode {
    if let Err(error) = confine() {
        // Nothing is read unconfined; the daemon sees the failure (stderr is for a person
        // running it by hand).
        eprintln!("edualc --decode-image: couldn't confine itself: {error}");
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
    let answer = if bytes.len() > MAX_INPUT_BYTES {
        Decoded::Refused(Refusal::TooManyBytes)
    } else {
        match decoded(&bytes, MAX_SIDE_PX) {
            Ok(picture) => Decoded::Image(picture),
            Err(refusal) => Decoded::Refused(refusal),
        }
    };
    let Ok(encoded) = postcard::to_stdvec(&answer) else {
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

/// What the helper makes of an image's bytes (sans-io: it runs in the helper, and in tests).
fn decoded(bytes: &[u8], max_side_px: u32) -> Result<Picture, Refusal> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| Refusal::Undecodable(error.to_string()))?;
    let came_as = match reader.format() {
        Some(ImageFormat::Png) => MediaType::Png,
        Some(ImageFormat::Jpeg) => MediaType::Jpeg,
        Some(_) | None => return Err(Refusal::NotPngOrJpeg),
    };
    let mut limits = Limits::default();
    limits.max_image_width = Some(max_side_px);
    limits.max_image_height = Some(max_side_px);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);
    let image = reader.decode().map_err(|error| match error {
        image::ImageError::Limits(_) => Refusal::TooLarge,
        other @ (image::ImageError::Decoding(_)
        | image::ImageError::Encoding(_)
        | image::ImageError::Parameter(_)
        | image::ImageError::Unsupported(_)
        | image::ImageError::IoError(_)) => Refusal::Undecodable(other.to_string()),
    })?;
    let (width_px, height_px) = sent_size(image.width(), image.height());
    let sent = if (width_px, height_px) == (image.width(), image.height()) {
        image
    } else {
        image.resize_exact(width_px, height_px, FilterType::Lanczos3)
    };
    let thumbnail = sent
        .thumbnail(THUMBNAIL_SIDE_PX, THUMBNAIL_SIDE_PX)
        .to_rgba8();
    let (media_type, bytes) = encoded(&sent, came_as)?;
    Ok(Picture {
        media_type,
        bytes,
        width_px,
        height_px,
        thumbnail: Thumbnail {
            width_px: thumbnail.width(),
            height_px: thumbnail.height(),
            rgba: thumbnail.into_raw(),
        },
    })
}

/// The image encoded as it came (a PNG stays exact); one too heavy that way goes as a JPEG.
fn encoded(image: &DynamicImage, came_as: MediaType) -> Result<(MediaType, Vec<u8>), Refusal> {
    let mut tries = vec![came_as];
    if came_as == MediaType::Png {
        tries.push(MediaType::Jpeg);
    }
    for media_type in tries {
        let mut bytes = Cursor::new(Vec::new());
        let written = match media_type {
            MediaType::Png => image.write_to(&mut bytes, ImageFormat::Png),
            // JPEG has no alpha.
            MediaType::Jpeg => {
                DynamicImage::ImageRgb8(image.to_rgb8()).write_to(&mut bytes, ImageFormat::Jpeg)
            }
        };
        written.map_err(|error| Refusal::Unencodable(error.to_string()))?;
        let bytes = bytes.into_inner();
        if bytes.len() <= MAX_SENT_BYTES {
            return Ok((media_type, bytes));
        }
    }
    Err(Refusal::TooHeavy)
}

/// The largest size, the aspect kept, within both of the current models' limits.
fn sent_size(width_px: u32, height_px: u32) -> (u32, u32) {
    let tokens = |width: u32, height: u32| {
        width.div_ceil(VISUAL_TOKEN_PX) * height.div_ceil(VISUAL_TOKEN_PX)
    };
    let long = width_px.max(height_px);
    if long <= SENT_LONG_EDGE_PX && tokens(width_px, height_px) <= SENT_VISUAL_TOKENS {
        return (width_px, height_px);
    }
    let scaled = |numerator: u64, denominator: u64| {
        let side = |side_px: u32| {
            let side = u64::from(side_px) * numerator / denominator;
            u32::try_from(side).unwrap_or(u32::MAX).max(1)
        };
        (side(width_px), side(height_px))
    };
    // The long edge first, then smaller steps while the tokens don't fit.
    let mut numerator = u64::from(SENT_LONG_EDGE_PX.min(long));
    let denominator = u64::from(long);
    let mut size = scaled(numerator, denominator);
    while tokens(size.0, size.1) > SENT_VISUAL_TOKENS && numerator > 1 {
        numerator -= 1;
        size = scaled(numerator, denominator);
    }
    size
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
    use super::*;

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

    #[test]
    fn what_isnt_a_png_or_jpeg_or_is_too_large_is_refused() {
        assert_eq!(
            decoded(b"GIF89a....", MAX_SIDE_PX),
            Err(Refusal::NotPngOrJpeg)
        );
        assert_eq!(
            decoded(b"plain text", MAX_SIDE_PX),
            Err(Refusal::NotPngOrJpeg)
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
                "decode::tests::a_confined_helper_is_killed_reaching_for_a_file",
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
