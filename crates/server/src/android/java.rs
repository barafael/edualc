//! What the app asks of Android itself, through JNI: where its files go, whether a keyboard is
//! attached, the clipboard (text copied, an image pasted), and notifications. Each call runs on
//! a thread of its own, attached to Java as it starts and detached as it ends, which frees what
//! Java made for it. What the window needs at once (the folder, the keyboard) is waited for and
//! is quick; the rest goes on beside the window, which hears of it, if at all, as it is done.
//!
//! The one place `unsafe` is let in (`unsafe_code` is denied elsewhere): Java's VM and the
//! activity come as raw pointers, which `ndk-context` holds from the moment the activity
//! started.

#![allow(unsafe_code)]

use jni::objects::{JObject, JString, JValue};
use jni::{JNIEnv, JavaVM};

/// Why Android didn't do what was asked.
#[derive(Debug, thiserror::Error)]
#[error("Android refused: {0}")]
pub struct Refused(String);

impl From<jni::errors::Error> for Refused {
    fn from(error: jni::errors::Error) -> Self {
        Self(error.to_string())
    }
}

/// Work with Java: the thread's Java environment and the activity.
trait Work<T>:
    for<'local> FnOnce(&mut JNIEnv<'local>, &JObject<'local>) -> Result<T, Refused> + Send + 'static
{
}

impl<T, F> Work<T> for F where
    F: for<'local> FnOnce(&mut JNIEnv<'local>, &JObject<'local>) -> Result<T, Refused>
        + Send
        + 'static
{
}

/// Does `work` with Java on this thread, which it attaches for the while.
fn attached<T>(work: impl Work<T>) -> Result<T, Refused> {
    let context = ndk_context::android_context();
    // SAFETY: `ndk-context` holds the process's Java VM, set as the activity started, and the
    // VM lives as long as the process; this handle doesn't destroy it as it drops.
    let vm = unsafe { JavaVM::from_raw(context.vm().cast()) }?;
    let mut env = vm.attach_current_thread()?;
    // SAFETY: `ndk-context` holds a global reference to the activity, valid while the app runs;
    // this handle doesn't delete it as it drops.
    let activity = unsafe { JObject::from_raw(context.context().cast()) };
    let done = work(&mut env, &activity);
    // What Java threw is said, and cleared before the thread detaches.
    if env.exception_check().unwrap_or(false)
        && (env.exception_describe().is_err() || env.exception_clear().is_err())
    {
        tracing::debug!("a Java exception stayed");
    }
    done
}

/// Does `work` with Java, on a thread of its own, and waits for it.
fn with_java<T: Send + 'static>(work: impl Work<T>) -> Result<T, Refused> {
    std::thread::spawn(move || attached(work))
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Does `work` with Java, on a thread of its own, and hands what came of it to `done` there.
fn beside<T: 'static>(work: impl Work<T>, done: impl FnOnce(Result<T, Refused>) + Send + 'static) {
    std::thread::spawn(move || done(attached(work)));
}

/// A system service, by its name (`Context.CLIPBOARD_SERVICE`'s, say).
fn service<'local>(
    env: &mut JNIEnv<'local>,
    activity: &JObject<'_>,
    name: &str,
) -> Result<JObject<'local>, Refused> {
    let name = env.new_string(name)?;
    let service = env
        .call_method(
            activity,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&name)],
        )?
        .l()?;
    if service.is_null() {
        return Err(Refused("no such service".to_owned()));
    }
    Ok(service)
}

/// A Java string as Rust's.
fn text(env: &mut JNIEnv<'_>, string: JObject<'_>) -> Result<String, Refused> {
    Ok(env.get_string(&JString::from(string))?.into())
}

/// Android's version (`Build.VERSION.SDK_INT`).
fn sdk(env: &mut JNIEnv<'_>) -> Result<i32, Refused> {
    Ok(env
        .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?
        .i()?)
}

/// The folder the app keeps its files in, its own, and kept out of Android's backups
/// (`Context.getNoBackupFilesDir`): a token there lets whoever holds it run Claude Code as the
/// user, and must not go to a cloud or to another phone with them.
pub fn files_dir() -> Result<std::path::PathBuf, Refused> {
    with_java(|env, activity| {
        let dir = env
            .call_method(activity, "getNoBackupFilesDir", "()Ljava/io/File;", &[])?
            .l()?;
        let path = env
            .call_method(&dir, "getAbsolutePath", "()Ljava/lang/String;", &[])?
            .l()?;
        Ok(text(env, path)?.into())
    })
}

/// Whether a keyboard with keys is attached and out (`Configuration.keyboard` is
/// `KEYBOARD_QWERTY`, and `hardKeyboardHidden` is `HARDKEYBOARDHIDDEN_NO`): Enter then sends,
/// as at a desk; on the screen's keyboard alone it starts a new line.
pub fn has_keyboard() -> Result<bool, Refused> {
    with_java(|env, activity| {
        let resources = env
            .call_method(
                activity,
                "getResources",
                "()Landroid/content/res/Resources;",
                &[],
            )?
            .l()?;
        let configuration = env
            .call_method(
                &resources,
                "getConfiguration",
                "()Landroid/content/res/Configuration;",
                &[],
            )?
            .l()?;
        let keyboard = env.get_field(&configuration, "keyboard", "I")?.i()?;
        let hidden = env
            .get_field(&configuration, "hardKeyboardHidden", "I")?
            .i()?;
        Ok(keyboard == 2 && hidden == 1)
    })
}

/// Puts `copied` on the clipboard.
pub fn copy(copied: String) {
    let work = move |env: &mut JNIEnv<'_>, activity: &JObject<'_>| {
        let manager = service(env, activity, "clipboard")?;
        let label = env.new_string("edualc")?;
        let copied = env.new_string(copied)?;
        let clip = env
            .call_static_method(
                "android/content/ClipData",
                "newPlainText",
                "(Ljava/lang/CharSequence;Ljava/lang/CharSequence;)Landroid/content/ClipData;",
                &[JValue::Object(&label), JValue::Object(&copied)],
            )?
            .l()?;
        env.call_method(
            &manager,
            "setPrimaryClip",
            "(Landroid/content/ClipData;)V",
            &[JValue::Object(&clip)],
        )?;
        Ok(())
    };
    beside(work, |copied| {
        if let Err(error) = copied {
            tracing::warn!(%error, "the clipboard refused the text");
        }
    });
}

/// The clipboard's image, its bytes as its file has them, up to `limit` (reading stops just
/// past it), or none if it holds no image (text, say, or nothing): handed to `done`.
pub fn clipboard_image(
    limit: usize,
    done: impl FnOnce(Result<Option<Vec<u8>>, Refused>) + Send + 'static,
) {
    let work = move |env: &mut JNIEnv<'_>, activity: &JObject<'_>| {
        let manager = service(env, activity, "clipboard")?;
        let clip = env
            .call_method(
                &manager,
                "getPrimaryClip",
                "()Landroid/content/ClipData;",
                &[],
            )?
            .l()?;
        if clip.is_null() || env.call_method(&clip, "getItemCount", "()I", &[])?.i()? < 1 {
            return Ok(None);
        }
        let item = env
            .call_method(
                &clip,
                "getItemAt",
                "(I)Landroid/content/ClipData$Item;",
                &[JValue::Int(0)],
            )?
            .l()?;
        let uri = env
            .call_method(&item, "getUri", "()Landroid/net/Uri;", &[])?
            .l()?;
        if uri.is_null() {
            return Ok(None);
        }
        let resolver = env
            .call_method(
                activity,
                "getContentResolver",
                "()Landroid/content/ContentResolver;",
                &[],
            )?
            .l()?;
        let kind = env
            .call_method(
                &resolver,
                "getType",
                "(Landroid/net/Uri;)Ljava/lang/String;",
                &[JValue::Object(&uri)],
            )?
            .l()?;
        if kind.is_null() || !text(env, kind)?.starts_with("image/") {
            return Ok(None);
        }
        let stream = env
            .call_method(
                &resolver,
                "openInputStream",
                "(Landroid/net/Uri;)Ljava/io/InputStream;",
                &[JValue::Object(&uri)],
            )?
            .l()?;
        let buffer = env.new_byte_array(64 * 1024)?;
        let mut bytes = Vec::new();
        while bytes.len() <= limit {
            let read = env
                .call_method(&stream, "read", "([B)I", &[JValue::Object(&buffer)])?
                .i()?;
            let Ok(read) = usize::try_from(read) else {
                // -1: the end.
                break;
            };
            let mut chunk = env.convert_byte_array(&buffer)?;
            chunk.truncate(read);
            bytes.extend_from_slice(&chunk);
        }
        env.call_method(&stream, "close", "()V", &[])?;
        Ok(Some(bytes))
    };
    beside(work, done);
}

/// The notification channel the app's notifications go to (Android 8 and later).
const CHANNEL: &str = "claude";
/// `NotificationManager.IMPORTANCE_DEFAULT`: a sound, no popping up.
const IMPORTANCE_DEFAULT: i32 = 3;
/// `PendingIntent.FLAG_IMMUTABLE | FLAG_UPDATE_CURRENT`.
const PENDING_FLAGS: i32 = 0x0400_0000 | 0x0800_0000;
/// A notification's builder, as its methods return it.
const BUILDER: &str = "Landroid/app/Notification$Builder;";
/// Android 13, from which notifying needs the user's leave (`POST_NOTIFICATIONS`).
const ASKS_TO_NOTIFY: i32 = 33;

/// Asks the user to let the app notify, on Android 13 and later, unless they did; as the
/// window opens, while the app is in front (Android shows no question from the background, and
/// asks no more once the user refused twice).
pub fn ask_to_notify() {
    let work = |env: &mut JNIEnv<'_>, activity: &JObject<'_>| {
        if sdk(env)? < ASKS_TO_NOTIFY || may_notify(env, activity)? {
            return Ok(());
        }
        let permission = env.new_string(POST_NOTIFICATIONS)?;
        let asked = env.new_object_array(1, "java/lang/String", &permission)?;
        env.call_method(
            activity,
            "requestPermissions",
            "([Ljava/lang/String;I)V",
            &[JValue::Object(&asked), JValue::Int(0)],
        )?;
        Ok(())
    };
    beside(work, |asked| {
        if let Err(error) = asked {
            tracing::warn!(%error, "couldn't ask to notify");
        }
    });
}

/// Shows a notification, which opens the app as it is tapped; one for each `summary` (a
/// session's), so that two sessions' don't replace each other. Not where the user didn't let
/// the app notify (see `ask_to_notify`).
pub fn notify(summary: String, body: String) {
    let id = notification_id(&summary);
    let work = move |env: &mut JNIEnv<'_>, activity: &JObject<'_>| {
        let sdk = sdk(env)?;
        if sdk >= ASKS_TO_NOTIFY && !may_notify(env, activity)? {
            return Ok(());
        }
        let manager = service(env, activity, "notification")?;
        let builder = if sdk >= 26 {
            let id = env.new_string(CHANNEL)?;
            let name = env.new_string("Claude Code")?;
            let channel = env.new_object(
                "android/app/NotificationChannel",
                "(Ljava/lang/String;Ljava/lang/CharSequence;I)V",
                &[
                    JValue::Object(&id),
                    JValue::Object(&name),
                    JValue::Int(IMPORTANCE_DEFAULT),
                ],
            )?;
            env.call_method(
                &manager,
                "createNotificationChannel",
                "(Landroid/app/NotificationChannel;)V",
                &[JValue::Object(&channel)],
            )?;
            env.new_object(
                "android/app/Notification$Builder",
                "(Landroid/content/Context;Ljava/lang/String;)V",
                &[JValue::Object(activity), JValue::Object(&id)],
            )?
        } else {
            env.new_object(
                "android/app/Notification$Builder",
                "(Landroid/content/Context;)V",
                &[JValue::Object(activity)],
            )?
        };
        // A status bar icon is drawn from its shape alone: the launcher's (an adaptive icon)
        // is a blot there, and Android 8.0 refuses it outright.
        let icon = env
            .get_static_field("android/R$drawable", "stat_notify_chat", "I")?
            .i()?;
        let summary = env.new_string(summary)?;
        let body = env.new_string(body)?;
        let opens = opens_the_app(env, activity)?;
        env.call_method(
            &builder,
            "setSmallIcon",
            format!("(I){BUILDER}"),
            &[JValue::Int(icon)],
        )?;
        env.call_method(
            &builder,
            "setContentTitle",
            format!("(Ljava/lang/CharSequence;){BUILDER}"),
            &[JValue::Object(&summary)],
        )?;
        env.call_method(
            &builder,
            "setContentText",
            format!("(Ljava/lang/CharSequence;){BUILDER}"),
            &[JValue::Object(&body)],
        )?;
        env.call_method(
            &builder,
            "setAutoCancel",
            format!("(Z){BUILDER}"),
            &[JValue::Bool(1)],
        )?;
        if !opens.is_null() {
            env.call_method(
                &builder,
                "setContentIntent",
                format!("(Landroid/app/PendingIntent;){BUILDER}"),
                &[JValue::Object(&opens)],
            )?;
        }
        let notification = env
            .call_method(&builder, "build", "()Landroid/app/Notification;", &[])?
            .l()?;
        env.call_method(
            &manager,
            "notify",
            "(ILandroid/app/Notification;)V",
            &[JValue::Int(id), JValue::Object(&notification)],
        )?;
        Ok(())
    };
    beside(work, |shown| {
        if let Err(error) = shown {
            tracing::warn!(%error, "Android showed no notification");
        }
    });
}

/// A notification's id for `summary`: the same for the same session's, which replaces its
/// last (FNV-1a, folded to an `int`).
fn notification_id(summary: &str) -> i32 {
    let hash = summary.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    i32::from_ne_bytes(hash.to_ne_bytes())
}

/// The permission to notify, by its name.
const POST_NOTIFICATIONS: &str = "android.permission.POST_NOTIFICATIONS";

/// Whether the user lets the app notify.
fn may_notify(env: &mut JNIEnv<'_>, activity: &JObject<'_>) -> Result<bool, Refused> {
    let permission = env.new_string(POST_NOTIFICATIONS)?;
    let granted = env
        .call_method(
            activity,
            "checkSelfPermission",
            "(Ljava/lang/String;)I",
            &[JValue::Object(&permission)],
        )?
        .i()?;
    // `PackageManager.PERMISSION_GRANTED`.
    Ok(granted == 0)
}

/// What opens the app, as a notification is tapped; null if Android names no way in.
fn opens_the_app<'local>(
    env: &mut JNIEnv<'local>,
    activity: &JObject<'_>,
) -> Result<JObject<'local>, Refused> {
    let package = env
        .call_method(activity, "getPackageName", "()Ljava/lang/String;", &[])?
        .l()?;
    let packages = env
        .call_method(
            activity,
            "getPackageManager",
            "()Landroid/content/pm/PackageManager;",
            &[],
        )?
        .l()?;
    let intent = env
        .call_method(
            &packages,
            "getLaunchIntentForPackage",
            "(Ljava/lang/String;)Landroid/content/Intent;",
            &[JValue::Object(&package)],
        )?
        .l()?;
    if intent.is_null() {
        return Ok(intent);
    }
    Ok(env
        .call_static_method(
            "android/app/PendingIntent",
            "getActivity",
            "(Landroid/content/Context;ILandroid/content/Intent;I)Landroid/app/PendingIntent;",
            &[
                JValue::Object(activity),
                JValue::Int(0),
                JValue::Object(&intent),
                JValue::Int(PENDING_FLAGS),
            ],
        )?
        .l()?)
}
