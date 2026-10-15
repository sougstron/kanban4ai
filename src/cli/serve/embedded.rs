//! One outbound sync owner per store, shared by serve, TUI and daemon.
use super::*;
use fs2::FileExt;
use std::fs::{File, OpenOptions};
use std::thread::{self, JoinHandle};

pub(crate) struct Embedded {
    stop: watch::Sender<bool>,
    thread: Option<JoinHandle<()>>,
}

fn try_owner(path: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e.into()),
    }
}

impl Embedded {
    pub(crate) fn start(root: &Path) -> Result<Self> {
        Self::spawn(root, false)
    }

    pub(crate) fn serving(root: &Path) -> Result<Self> {
        Self::spawn(root, true)
    }

    fn spawn(root: &Path, serving: bool) -> Result<Self> {
        // A disabled store must not create sync/ or take serve.lock. The
        // singleton error is synchronous only once sync is already on.
        let service = if serving && crate::core::sync::enabled(root) {
            fs::create_dir_all(root.join("sync"))?;
            Some(
                try_owner(&root.join("sync/serve.lock"))?
                    .ok_or_else(|| invalid("serve is already running"))?,
            )
        } else {
            None
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?;
        let root = root.to_path_buf();
        let (stop, mut stopped) = watch::channel(false);
        let thread = thread::Builder::new().name("sync".into()).spawn(move || {
            runtime.block_on(async move {
                let mut running: Option<sync::Running> = None;
                let mut ownership: Option<File> = None;
                let mut service = service;
                loop {
                    if !crate::core::sync::enabled(&root) {
                        if let Some(worker) = running.take() {
                            worker.stop().await;
                        }
                        ownership.take();
                        service.take();
                        tokio::select! {
                            _ = stopped.changed() => break,
                            _ = tokio::time::sleep(Duration::from_secs(2)) => (),
                        }
                        continue;
                    }
                    if serving && service.is_none() {
                        match (|| -> Result<Option<File>> {
                            fs::create_dir_all(root.join("sync"))?;
                            try_owner(&root.join("sync/serve.lock"))
                        })() {
                            Ok(Some(file)) => service = Some(file),
                            Ok(None) => {
                                eprintln!("serve is already running");
                                break;
                            }
                            Err(e) => eprintln!("sync owner: {e}"),
                        }
                    }
                    let config = discovery::config(&root);
                    // Serve takes precedence even if a TUI/daemon started first.
                    let service_active = !serving
                        && fs::create_dir_all(root.join("sync")).is_ok()
                        && try_owner(&root.join("sync/serve.lock"))
                            .is_ok_and(|lock| lock.is_none());
                    if service_active {
                        if let Some(worker) = running.take() {
                            worker.stop().await;
                            ownership.take();
                        }
                    }
                    if !service_active
                        && running.is_none()
                        && config.as_ref().is_ok_and(|c| !c.peers.is_empty())
                    {
                        let acquired = try_owner(&root.join("sync/worker.lock"));
                        match acquired {
                            Ok(Some(file)) => {
                                ownership = Some(file);
                                if let Err(e) = discovery::notify(&root, None).await {
                                    eprintln!("sync hello: {e}");
                                }
                                running = Some(sync::start(&root));
                            }
                            Ok(None) => (),
                            Err(e) => eprintln!("sync owner: {e}"),
                        }
                    }
                    let interval = config.map_or(30, |c| c.interval);
                    tokio::select! {
                        _ = stopped.changed() => break,
                        _ = tokio::time::sleep(Duration::from_secs(interval)) => (),
                    }
                }
                if let Some(running) = running {
                    running.stop().await;
                    if let Err(e) = discovery::notify(&root, Some("shutdown")).await {
                        eprintln!("sync bye: {e}");
                    }
                } else if crate::core::sync::enabled(&root) {
                    // A TUI using another process's sync worker still checkpoints
                    // on exit, but must not announce that the live node went away.
                    let interval = discovery::config(&root).map_or(60, |c| c.run_heartbeat);
                    match tokio::task::spawn_blocking(move || {
                        crate::core::wip::publish(&root, interval, true)
                    })
                    .await
                    {
                        Ok(Ok(())) => (),
                        Ok(Err(e)) => eprintln!("WIP on exit: {e}"),
                        Err(e) => eprintln!("WIP on exit: {e}"),
                    }
                }
                drop(ownership);
                drop(service);
            });
        })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Embedded {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
