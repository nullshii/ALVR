//! Copies screenshots taken with the headset own capture button over to the streaming PC.
//!
//! The headset system UI writes the capture itself; ALVR never renders or encodes it. We only
//! notice the new file and forward its bytes unchanged.
//!
//! Access goes through `MediaStore` rather than the file path. Pico 4 runs API 29, where scoped
//! storage is enforced and direct file path access to another app media was not yet available
//! (Android 11 introduced it). `requestLegacyExternalStorage` is not an option either: the
//! cargo-apk fork ALVR builds with cannot emit that manifest attribute.

use crate::{ClientCoreEvent, connection::ConnectionContext};
use alvr_common::{HAND_LEFT_ID, HAND_RIGHT_ID, info, parking_lot::Mutex};
use alvr_packets::{ClientControlPacket, SCREENSHOT_CHUNK_SIZE, ScreenshotChunk, ScreenshotStart};
use std::{collections::VecDeque, time::Duration};

/// How often MediaStore is queried while streaming. The system needs a moment to finish writing
/// and index the capture anyway, so a tighter interval would only burn battery.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Feedback pulse once a screenshot has reached the PC. The system already plays its own shutter
/// sound at capture time, so this signals delivery, not capture.
const HAPTIC_DURATION: Duration = Duration::from_millis(50);
const HAPTIC_FREQUENCY: f32 = 160.0;
const HAPTIC_AMPLITUDE: f32 = 0.7;
/// Left, then right: the direction is what makes the pulse readable as an ALVR notification
/// rather than stray in-game rumble.
const HAPTIC_HAND_DELAY: Duration = Duration::from_millis(80);

/// A capture found on the headset, already read into memory.
pub struct FoundScreenshot {
    pub display_name: String,
    pub extension: String,
    pub data: Vec<u8>,
}

/// Extensions we forward. Videos share the same folder and must not be picked up: they are a
/// different size class and would sit on the control socket far too long.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const ALLOWED_EXTENSIONS: [&str; 3] = ["jpeg", "jpg", "png"];

/// Decides whether a MediaStore entry is a screenshot we should forward, returning its extension.
///
/// Deliberately vendor-agnostic. Pico embeds the foreground app package in the file name
/// (`Screenshot_alvr.client.dev_2026.09.02-11.35.29.451_800.jpeg`), which would identify captures
/// taken over ALVR exactly, but Quest names its files differently and a strict package match
/// there would silently forward nothing. Since the entries are already restricted to those
/// created while streaming, mime type and extension are enough to decide; the package match is
/// kept as a diagnostic in [`matches_package`].
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn screenshot_extension(display_name: &str, mime_type: &str) -> Option<String> {
    let extension = display_name.rsplit_once('.')?.1.to_lowercase();

    if !ALLOWED_EXTENSIONS.contains(&extension.as_str()) {
        return None;
    }

    let mime_type = mime_type.to_lowercase();
    if !matches!(mime_type.as_str(), "image/jpeg" | "image/png") {
        return None;
    }

    Some(extension)
}

/// True when the file name carries our own package id, as Pico writes it.
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub fn matches_package(display_name: &str, package_name: &str) -> bool {
    !package_name.is_empty() && display_name.contains(package_name)
}

/// Splits `data` into control-socket sized chunks.
///
/// Chunking is a protocol requirement, not an optimisation: the server refreshes its keepalive
/// deadline only between whole packets, and its timeout is 2 seconds. A one-megabyte capture sent
/// as a single packet would routinely be read as a dropped client.
pub fn chunk_count(len: usize) -> u32 {
    len.div_ceil(SCREENSHOT_CHUNK_SIZE).max(1) as u32
}

/// Sends one screenshot over the control socket. Returns false if the connection went away
/// mid-transfer, in which case the partial transfer dies with the server side assembler.
pub fn send_screenshot(ctx: &ConnectionContext, id: u32, screenshot: &FoundScreenshot) -> bool {
    if screenshot.data.is_empty() {
        // Can happen if the capture was still being written when we read it. The server rejects
        // empty transfers anyway; bailing here avoids announcing a transfer that never completes.
        return false;
    }

    let chunk_count = chunk_count(screenshot.data.len());

    let start = ClientControlPacket::ScreenshotStart(ScreenshotStart {
        id,
        extension: screenshot.extension.clone(),
        chunk_count,
        total_size: screenshot.data.len() as u64,
    });

    if let Some(sender) = &mut *ctx.control_sender.lock() {
        if sender.send(&start).is_err() {
            return false;
        }
    } else {
        return false;
    }

    for (chunk_index, chunk) in screenshot.data.chunks(SCREENSHOT_CHUNK_SIZE).enumerate() {
        let packet = ClientControlPacket::ScreenshotChunk(ScreenshotChunk {
            id,
            chunk_index: chunk_index as u32,
            data: chunk.to_vec(),
        });

        // Re-locked per chunk so keepalives and tracking are not starved during the transfer.
        if let Some(sender) = &mut *ctx.control_sender.lock() {
            if sender.send(&packet).is_err() {
                return false;
            }
        } else {
            return false;
        }
    }

    info!(
        "Sent headset screenshot to PC: {} ({} bytes)",
        screenshot.display_name,
        screenshot.data.len()
    );

    true
}

/// Queues the delivery pulse. Called from the screenshot thread, which owns the delay between
/// hands; the event queue itself is drained once per frame by the OpenXR layer.
pub fn notify_delivered(event_queue: &Mutex<VecDeque<ClientCoreEvent>>) {
    for (index, device_id) in [*HAND_LEFT_ID, *HAND_RIGHT_ID].into_iter().enumerate() {
        if index > 0 {
            std::thread::sleep(HAPTIC_HAND_DELAY);
        }

        event_queue.lock().push_back(ClientCoreEvent::Haptics {
            device_id,
            duration: HAPTIC_DURATION,
            frequency: HAPTIC_FREQUENCY,
            amplitude: HAPTIC_AMPLITUDE,
        });
    }
}

/// Whether the headset granted us access to the media collection. Reported to the user rather
/// than failing silently: a denied dialog is otherwise indistinguishable from a broken feature.
pub fn storage_permission_granted() -> bool {
    #[cfg(target_os = "android")]
    {
        alvr_system_info::has_permission(alvr_system_info::STORAGE_READ_PERMISSION)
            || alvr_system_info::has_permission(alvr_system_info::MEDIA_IMAGES_PERMISSION)
    }
    #[cfg(not(target_os = "android"))]
    {
        false
    }
}

#[cfg(target_os = "android")]
pub use android::ScreenshotWatcher;

#[cfg(not(target_os = "android"))]
pub use stub::ScreenshotWatcher;

/// Non-Android clients have no headset capture button; the watcher is a no-op there.
#[cfg(not(target_os = "android"))]
mod stub {
    use super::FoundScreenshot;

    pub struct ScreenshotWatcher;

    impl ScreenshotWatcher {
        pub fn new() -> Self {
            Self
        }

        pub fn poll(&mut self) -> Vec<FoundScreenshot> {
            Vec::new()
        }
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::{FoundScreenshot, matches_package, screenshot_extension};
    use alvr_common::{debug, warn};
    use alvr_system_info::{context, vm};
    use jni::{
        Env,
        errors::Result as JniResult,
        jni_sig, jni_str,
        objects::{JObject, JString},
        refs::Reference,
    };
    use std::{
        fs::File,
        io::Read,
        os::fd::FromRawFd,
        time::{SystemTime, UNIX_EPOCH},
    };

    /// MediaStore columns we read, in query order.
    const COLUMN_ID: i32 = 0;
    const COLUMN_DISPLAY_NAME: i32 = 1;
    const COLUMN_MIME_TYPE: i32 = 2;
    const COLUMN_DATE_ADDED: i32 = 3;

    /// Guards against a runaway query if the media collection is unexpectedly large.
    const MAX_ENTRIES_PER_POLL: usize = 16;

    /// Watches the shared image collection for captures created after it was constructed.
    ///
    /// Only entries newer than the watermark are considered, so a screenshot taken before the
    /// stream started is never uploaded and each capture is sent exactly once.
    pub struct ScreenshotWatcher {
        /// `MediaStore.Images.Media.DATE_ADDED`, in whole seconds since the epoch.
        watermark_secs: i64,
        package_name: String,
    }

    impl ScreenshotWatcher {
        pub fn new() -> Self {
            let watermark_secs = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);

            Self {
                watermark_secs,
                package_name: package_name().unwrap_or_default(),
            }
        }

        pub fn poll(&mut self) -> Vec<FoundScreenshot> {
            match self.query() {
                Ok(screenshots) => screenshots,
                Err(e) => {
                    // Most likely the storage permission was denied; already reported at startup.
                    debug!("MediaStore screenshot query failed: {e:?}");

                    Vec::new()
                }
            }
        }

        fn query(&mut self) -> JniResult<Vec<FoundScreenshot>> {
            let watermark = self.watermark_secs;
            let package_name = self.package_name.clone();

            let (screenshots, new_watermark) = vm().attach_current_thread(|env| {
                let resolver = env
                    .call_method(
                        unsafe { JObject::global_kind_from_raw(context()) },
                        jni_str!("getContentResolver"),
                        jni_sig!("()Landroid/content/ContentResolver;"),
                        &[],
                    )?
                    .l()?;

                let collection = env
                    .get_static_field(
                        jni_str!("android/provider/MediaStore$Images$Media"),
                        jni_str!("EXTERNAL_CONTENT_URI"),
                        jni_sig!("Landroid/net/Uri;"),
                    )?
                    .l()?;

                let projection = {
                    let id = env.new_string("_id")?;
                    let array = env.new_object_array(4, jni_str!("java/lang/String"), &id)?;

                    let display_name = env.new_string("_display_name")?;
                    array.set_element(env, 1, &display_name)?;
                    let mime_type = env.new_string("mime_type")?;
                    array.set_element(env, 2, &mime_type)?;
                    let date_added = env.new_string("date_added")?;
                    array.set_element(env, 3, &date_added)?;

                    array
                };

                let selection = env.new_string("date_added > ?")?;
                let selection_args = {
                    let value = env.new_string(watermark.to_string())?;

                    env.new_object_array(1, jni_str!("java/lang/String"), &value)?
                };
                let sort_order = env.new_string("date_added ASC")?;

                let cursor = env
                    .call_method(
                        &resolver,
                        jni_str!("query"),
                        jni_sig!(
                            "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;\
                             [Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;"
                        ),
                        &[
                            (&collection).into(),
                            (&projection).into(),
                            (&selection).into(),
                            (&selection_args).into(),
                            (&sort_order).into(),
                        ],
                    )?
                    .l()?;

                if cursor.is_null() {
                    return JniResult::Ok((Vec::new(), watermark));
                }

                let mut screenshots = Vec::new();
                let mut new_watermark = watermark;

                while screenshots.len() < MAX_ENTRIES_PER_POLL
                    && env
                        .call_method(&cursor, jni_str!("moveToNext"), jni_sig!("()Z"), &[])?
                        .z()?
                {
                    let date_added = env
                        .call_method(
                            &cursor,
                            jni_str!("getLong"),
                            jni_sig!("(I)J"),
                            &[COLUMN_DATE_ADDED.into()],
                        )?
                        .j()?;

                    // Advanced even for skipped entries: a file we reject must not be re-examined
                    // on every single poll for the rest of the session.
                    new_watermark = new_watermark.max(date_added);

                    let display_name = {
                        let value = env
                            .call_method(
                                &cursor,
                                jni_str!("getString"),
                                jni_sig!("(I)Ljava/lang/String;"),
                                &[COLUMN_DISPLAY_NAME.into()],
                            )?
                            .l()?;

                        if value.is_null() {
                            continue;
                        }

                        env.cast_local::<JString>(value)?.to_string()
                    };

                    let mime_type = {
                        let value = env
                            .call_method(
                                &cursor,
                                jni_str!("getString"),
                                jni_sig!("(I)Ljava/lang/String;"),
                                &[COLUMN_MIME_TYPE.into()],
                            )?
                            .l()?;

                        if value.is_null() {
                            String::new()
                        } else {
                            env.cast_local::<JString>(value)?.to_string()
                        }
                    };

                    let Some(extension) = screenshot_extension(&display_name, &mime_type) else {
                        continue;
                    };

                    debug!(
                        "Found headset screenshot {display_name} (package match: {})",
                        matches_package(&display_name, &package_name)
                    );

                    let id = env
                        .call_method(
                            &cursor,
                            jni_str!("getLong"),
                            jni_sig!("(I)J"),
                            &[COLUMN_ID.into()],
                        )?
                        .j()?;

                    match read_entry(env, &resolver, &collection, id) {
                        Ok(data) => screenshots.push(FoundScreenshot {
                            display_name,
                            extension,
                            data,
                        }),
                        Err(e) => warn!("Could not read headset screenshot {display_name}: {e:?}"),
                    }
                }

                env.call_method(&cursor, jni_str!("close"), jni_sig!("()V"), &[])?;

                JniResult::Ok((screenshots, new_watermark))
            })?;

            self.watermark_secs = new_watermark;

            Ok(screenshots)
        }
    }

    /// Reads one media entry through the content resolver.
    ///
    /// `openFileDescriptor` hands back a real fd, so the bytes are read with std rather than
    /// shuttled through JNI byte arrays.
    fn read_entry(
        env: &mut Env<'_>,
        resolver: &JObject,
        collection: &JObject,
        id: i64,
    ) -> JniResult<Vec<u8>> {
        let item_uri = env
            .call_static_method(
                jni_str!("android/content/ContentUris"),
                jni_str!("withAppendedId"),
                jni_sig!("(Landroid/net/Uri;J)Landroid/net/Uri;"),
                &[collection.into(), id.into()],
            )?
            .l()?;

        let mode = env.new_string("r")?;
        let descriptor = env
            .call_method(
                resolver,
                jni_str!("openFileDescriptor"),
                jni_sig!("(Landroid/net/Uri;Ljava/lang/String;)Landroid/os/ParcelFileDescriptor;"),
                &[(&item_uri).into(), (&mode).into()],
            )?
            .l()?;

        // detachFd() transfers ownership to us, so the File below is what closes it.
        let raw_fd = env
            .call_method(&descriptor, jni_str!("detachFd"), jni_sig!("()I"), &[])?
            .i()?;

        let mut file = unsafe { File::from_raw_fd(raw_fd) };
        let mut data = Vec::new();
        file.read_to_end(&mut data).ok();

        Ok(data)
    }

    /// Our own APK id, used to recognise captures taken while ALVR was in the foreground.
    fn package_name() -> Option<String> {
        vm().attach_current_thread(|env| {
            let name = env
                .call_method(
                    unsafe { JObject::global_kind_from_raw(context()) },
                    jni_str!("getPackageName"),
                    jni_sig!("()Ljava/lang/String;"),
                    &[],
                )?
                .l()?;

            JniResult::Ok(env.cast_local::<JString>(name)?.to_string())
        })
        .ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PICO_NAME: &str = "Screenshot_alvr.client.dev_2026.09.02-11.35.29.451_800.jpeg";

    #[test]
    fn accepts_pico_screenshot() {
        let extension = screenshot_extension(PICO_NAME, "image/jpeg");

        assert_eq!(extension.as_deref(), Some("jpeg"));
        assert!(matches_package(PICO_NAME, "alvr.client.dev"));
    }

    #[test]
    fn accepts_png_from_unknown_vendor_naming() {
        // Quest uses a different convention; falling back to mime and extension keeps it working.
        let extension = screenshot_extension("capture-001.PNG", "image/png");

        assert_eq!(extension.as_deref(), Some("png"));
        assert!(!matches_package("capture-001.PNG", "alvr.client.dev"));
    }

    #[test]
    fn rejects_video_recordings() {
        // The headset writes screen recordings into the same collection.
        assert!(screenshot_extension("Record_1.mp4", "video/mp4").is_none());
    }

    #[test]
    fn rejects_mime_and_extension_mismatch() {
        assert!(screenshot_extension("fake.jpeg", "video/mp4").is_none());
        assert!(screenshot_extension("fake.webp", "image/jpeg").is_none());
    }

    #[test]
    fn rejects_name_without_extension() {
        assert!(screenshot_extension("screenshot", "image/jpeg").is_none());
    }

    #[test]
    fn empty_package_never_matches() {
        assert!(!matches_package(PICO_NAME, ""));
    }

    #[test]
    fn chunk_count_covers_all_bytes() {
        assert_eq!(chunk_count(0), 1);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(SCREENSHOT_CHUNK_SIZE), 1);
        assert_eq!(chunk_count(SCREENSHOT_CHUNK_SIZE + 1), 2);
        // A typical Pico capture is around one megabyte.
        assert_eq!(chunk_count(1_259_464), 5);
    }
}
