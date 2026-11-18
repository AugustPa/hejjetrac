//! Nonblocking atlas file selection; image I/O remains on the tracking worker.
use crate::{runtime::Command, session::Session};
use eframe::egui;
use std::{
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll, Wake, Waker},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    Export,
    Import,
}
impl Operation {
    fn label(self) -> &'static str {
        match self {
            Self::Export => "Export atlas PNG",
            Self::Import => "Import atlas PNG",
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct SelectionGuard {
    revision: u64,
    config: String,
    config_path: Option<PathBuf>,
    source_base: Option<PathBuf>,
    source: String,
    camera_binding: Option<String>,
}
impl SelectionGuard {
    fn capture(session: &Session) -> Self {
        Self {
            revision: session.revision,
            config: session.history.text.clone(),
            config_path: session.path.clone(),
            source_base: session.source_base.clone(),
            source: session.running_source.clone(),
            camera_binding: session.running_camera_binding.clone(),
        }
    }
}

type Selection = Pin<Box<dyn Future<Output = Option<PathBuf>> + Send>>;
struct Pending {
    operation: Operation,
    guard: SelectionGuard,
    selection: Selection,
    waker: Waker,
}

#[derive(Default)]
pub(crate) struct AtlasDialog {
    builder: rfd::AsyncFileDialog,
    pending: Option<Pending>,
    pub path: String,
    pub show_path: bool,
    pub notice: String,
}
impl AtlasDialog {
    pub fn set_parent(&mut self, frame: &eframe::Frame) {
        self.builder = rfd::AsyncFileDialog::new().set_parent(frame);
    }
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }
    pub fn discard_pending(&mut self) {
        self.pending = None;
    }
    pub fn abandon(&mut self) {
        self.pending = None;
        self.show_path = true;
        self.notice =
            "Stopped waiting here. Close the separate file chooser if it is still open.".into();
    }
    pub fn suggest_path(&mut self, path: PathBuf) {
        if !self.path.trim().is_empty() {
            return;
        }
        self.path = path.to_string_lossy().into_owned();
    }
    fn poll(&mut self, guard: &SelectionGuard) -> Option<Result<(Operation, PathBuf), String>> {
        let pending = self.pending.as_mut()?;
        if &pending.guard != guard {
            self.pending = None;
            return Some(Err(
                "Atlas file selection discarded because this rig's source or configuration changed."
                    .into(),
            ));
        }
        let mut context = Context::from_waker(&pending.waker);
        let Poll::Ready(path) = pending.selection.as_mut().poll(&mut context) else {
            return None;
        };
        let operation = pending.operation;
        self.pending = None;
        match path {
            Some(path) => {
                self.path = path.to_string_lossy().into_owned();
                self.notice.clear();
                Some(Ok((operation, path)))
            }
            None => {
                self.notice = "File selection cancelled.".into();
                None
            }
        }
    }
}

struct Repaint(egui::Context);
impl Wake for Repaint {
    fn wake(self: Arc<Self>) {
        self.0.request_repaint();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.request_repaint();
    }
}

impl Session {
    pub(crate) fn default_atlas_directories(&self) -> Vec<PathBuf> {
        // Resolve the portable-package location once, rather than consulting
        // the filesystem during every settings repaint. Protected application
        // folders are handled by the worker's personal-folder fallback.
        static DIRECTORIES: OnceLock<Vec<PathBuf>> = OnceLock::new();
        DIRECTORIES
            .get_or_init(|| {
                let personal = crate::runtime::recording_root().join("atlases");
                let portable = std::env::current_exe()
                    .ok()
                    .and_then(|exe| exe.parent().map(Path::to_owned))
                    .filter(|folder| folder.join("BUILD.json").is_file())
                    .map(|folder| folder.join("atlases"));
                let mut directories = Vec::with_capacity(2);
                if let Some(portable) = portable {
                    directories.push(portable);
                }
                if !directories.contains(&personal) {
                    directories.push(personal);
                }
                directories
            })
            .clone()
    }
    pub(crate) fn suggest_atlas_path(&mut self) {
        let directory = self
            .default_atlas_directories()
            .into_iter()
            .next()
            .unwrap_or_else(|| crate::runtime::recording_root().join("atlases"));
        self.atlas_dialog.suggest_path(directory.join(format!(
            "{}.png",
            hejjetrac_io::atlas_filename_stem(&self.display_name())
        )));
    }
    pub(crate) fn save_default_atlas(&mut self) -> Result<(), String> {
        let worker = self
            .worker
            .as_ref()
            .ok_or("Start this rig before saving its atlas.")?;
        worker.send(Command::SaveMapDefault {
            directories: self.default_atlas_directories(),
            name: self.display_name(),
        })?;
        self.atlas_dialog.discard_pending();
        self.atlas_dialog.notice = "Atlas PNG save queued.".into();
        Ok(())
    }
    pub(crate) fn choose_atlas_file(&mut self, operation: Operation, ctx: &egui::Context) {
        if self.atlas_dialog.pending() {
            return;
        }
        self.suggest_atlas_path();
        let path = PathBuf::from(&self.atlas_dialog.path);
        if self.atlas_dialog.path.contains('\0') {
            self.error = "PNG filenames cannot contain a null character.".into();
            return;
        }
        let mut builder = self
            .atlas_dialog
            .builder
            .clone()
            .add_filter("PNG", &["png"])
            .set_title(operation.label());
        if let Some(directory) = path.parent() {
            builder = builder.set_directory(directory);
        }
        if operation == Operation::Export
            && let Some(name) = path.file_name()
        {
            builder = builder.set_file_name(name.to_string_lossy());
        }
        let selection: Selection = Box::pin(async move {
            let file = match operation {
                Operation::Export => builder.save_file().await,
                Operation::Import => builder.pick_file().await,
            };
            file.map(|file| file.path().to_owned())
        });
        self.atlas_dialog.notice.clear();
        self.atlas_dialog.pending = Some(Pending {
            operation,
            guard: SelectionGuard::capture(self),
            selection,
            waker: Waker::from(Arc::new(Repaint(ctx.clone()))),
        });
        ctx.request_repaint();
    }
    pub(crate) fn poll_atlas_file(&mut self) {
        if !self.atlas_dialog.pending() {
            return;
        }
        let guard = SelectionGuard::capture(self);
        if let Some(result) = self.atlas_dialog.poll(&guard) {
            let result =
                result.and_then(|(operation, path)| self.apply_atlas_file(operation, path));
            self.report(result);
        }
    }
    pub(crate) fn atlas_file_from_path(&mut self, operation: Operation) -> Result<(), String> {
        // Also invalidates a native chooser result that may arrive later.
        self.atlas_dialog.pending = None;
        let text = self.atlas_dialog.path.trim();
        if text.is_empty() {
            return Err("Enter a PNG filename, including its folder.".into());
        }
        let path = if let Some(tail) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\"))
        {
            let home = std::env::var_os("USERPROFILE")
                .or_else(|| std::env::var_os("HOME"))
                .ok_or("Cannot resolve ~; enter the full PNG path.")?;
            PathBuf::from(home).join(tail)
        } else {
            PathBuf::from(text)
        };
        self.apply_atlas_file(operation, path)
    }
    fn apply_atlas_file(&mut self, operation: Operation, mut path: PathBuf) -> Result<(), String> {
        if operation == Operation::Export && path.extension().is_none() {
            path.set_extension("png");
        }
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("png"))
        {
            return Err("Choose a filename ending in .png.".into());
        }
        let worker = self
            .worker
            .as_ref()
            .ok_or("Start this rig before using an atlas image.")?;
        let command = match operation {
            Operation::Export => Command::SaveMap(path.clone()),
            Operation::Import => Command::LoadMap(path.clone()),
        };
        worker.send(command)?;
        self.atlas_dialog.path = path.to_string_lossy().into_owned();
        self.atlas_dialog.notice = format!("{} queued: {}", operation.label(), path.display());
        if operation == Operation::Import {
            self.texture_frame = u64::MAX;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{Snapshot, Worker};
    use hejjetrac_io::ConfigDocument;
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };
    type Result = std::result::Result<(), Box<dyn std::error::Error>>;

    fn session() -> std::result::Result<Session, Box<dyn std::error::Error>> {
        Ok(Session::new(
            1,
            ConfigDocument::parse("src_fn:sample.mp4\nzmq_port:0\nsock_port:0\n".into())?,
            None,
        ))
    }
    fn pending(session: &Session, selection: Selection, ctx: &egui::Context) -> Pending {
        Pending {
            operation: Operation::Export,
            guard: SelectionGuard::capture(session),
            selection,
            waker: Waker::from(Arc::new(Repaint(ctx.clone()))),
        }
    }

    #[test]
    fn pending_chooser_keeps_updates_and_rendering_alive_with_settings_closed() -> Result {
        let mut session = session()?;
        session.worker = Some(Worker::disconnected_for_test(Snapshot {
            frames: 99,
            ..Snapshot::default()
        }));
        session.show_settings = false;
        let polls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&polls);
        let selection = Box::pin(std::future::poll_fn(move |_| {
            calls.fetch_add(1, Ordering::Relaxed);
            Poll::Pending
        }));
        let ctx = egui::Context::default();
        session.atlas_dialog.pending = Some(pending(&session, selection, &ctx));
        for _ in 0..3 {
            let output = ctx.run(egui::RawInput::default(), |ctx| session.show(ctx));
            assert!(!output.shapes.is_empty());
            assert_eq!(session.snapshot.frames, 99);
            assert!(session.atlas_dialog.pending());
        }
        assert_eq!(polls.load(Ordering::Relaxed), 3);
        assert!(session.error.is_empty());
        Ok(())
    }

    #[test]
    fn abandoned_and_stale_chooser_results_cannot_send_atlas_commands() -> Result {
        let ctx = egui::Context::default();
        for change in 0..4 {
            let mut session = session()?;
            session.worker = Some(Worker::disconnected_for_test(Snapshot::default()));
            let polls = Arc::new(AtomicUsize::new(0));
            let calls = Arc::clone(&polls);
            let selection = Box::pin(std::future::poll_fn(move |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Poll::Ready(Some(PathBuf::from("must-not-be-exported.png")))
            }));
            session.atlas_dialog.pending = Some(pending(&session, selection, &ctx));
            match change {
                0 => session.atlas_dialog.abandon(),
                1 => session.revision += 1,
                2 => session.history.replace("src_fn:different.mp4\n".into()),
                _ => session.running_source = "different.mp4".into(),
            }
            session.update();
            assert!(!session.atlas_dialog.pending());
            assert_eq!(polls.load(Ordering::Relaxed), 0);
            if change == 0 {
                assert!(session.error.is_empty());
                assert!(session.atlas_dialog.show_path);
            } else {
                assert!(session.error.contains("selection discarded"));
            }
            assert!(session.atlas_dialog.path.is_empty());
        }
        // A user-cancelled native chooser is also a no-op, rather than an
        // attempt to export to an empty path or a worker-disconnection error.
        let mut session = session()?;
        session.worker = Some(Worker::disconnected_for_test(Snapshot::default()));
        session.atlas_dialog.pending = Some(pending(&session, Box::pin(async { None }), &ctx));
        session.update();
        assert!(!session.atlas_dialog.pending());
        assert!(session.error.is_empty());
        assert_eq!(session.atlas_dialog.notice, "File selection cancelled.");
        Ok(())
    }

    #[test]
    fn selected_and_direct_png_paths_export_real_worker_atlas_without_clock_or_config_edits()
    -> Result {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut doc = ConfigDocument::parse(std::fs::read_to_string(
            root.join("examples/upstream-config.txt"),
        )?)?;
        doc.set(
            "src_fn",
            &root.join("examples/sample.mp4").to_string_lossy(),
        )?;
        doc.set("zmq_port", "0")?;
        doc.set("sock_port", "0")?;
        let mut session = Session::new(1, doc, None);
        session.realtime = false;
        session.start()?;
        session.send(Command::Pause(true));
        session.send(Command::Advance(3));
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            session.update();
            if session.snapshot.frames >= 3
                && session.snapshot.paused
                && session.snapshot.advance_remaining == 0
            {
                break;
            }
            if Instant::now() > deadline {
                return Err("sample worker did not pause after visible frames".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let before = serde_json::to_value(&session.snapshot.row)?;
        let expected_pixels = session
            .snapshot
            .image
            .as_ref()
            .and_then(|image| image.debug.as_ref())
            .ok_or("sample atlas snapshot missing")?
            .accumulated
            .clone();
        let config = session.history.text.clone();
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "hejjetrac-atlas-dialog-{}-{timestamp}",
            std::process::id()
        ));
        std::fs::create_dir(&directory)?;
        let ctx = egui::Context::default();
        let chosen = directory.join("chosen.png");
        let selected = chosen.clone();
        session.atlas_dialog.pending = Some(pending(
            &session,
            Box::pin(async move { Some(selected) }),
            &ctx,
        ));
        session.update();
        let direct = directory.join("direct.png");
        session.atlas_dialog.path = direct.to_string_lossy().into_owned();
        session.atlas_file_from_path(Operation::Export)?;
        for path in [&chosen, &direct] {
            loop {
                session.update();
                if let Ok(map) = hejjetrac_io::load_sphere_map(path) {
                    assert_eq!(map.pixels, expected_pixels);
                    break;
                }
                if Instant::now() > deadline {
                    return Err(format!(
                        "atlas export did not complete: {}",
                        session.snapshot.notice
                    )
                    .into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert_eq!(session.history.text, config);
        assert_eq!(serde_json::to_value(&session.snapshot.row)?, before);
        assert!(session.snapshot.paused);
        let bytes = std::fs::read(&chosen)?;
        session.atlas_dialog.path = chosen.to_string_lossy().into_owned();
        session.atlas_file_from_path(Operation::Export)?;
        loop {
            session.update();
            if session.snapshot.notice.contains("exists") {
                break;
            }
            if Instant::now() > deadline {
                return Err("existing atlas output was not rejected".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read(&chosen)?, bytes);
        session.worker = None;
        std::fs::remove_dir_all(directory)?;
        Ok(())
    }
}
