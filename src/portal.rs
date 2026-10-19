//! Global hotkeys via the XDG GlobalShortcuts portal (works on GNOME, KDE
//! and niri >= 25.02). Best-effort: on compositors without the portal the
//! app reports it and users fall back to compositor keybinds / the CLI.

use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use futures_util::StreamExt;

pub enum PortalEvent {
    /// (message, ok)
    Status(String, bool),
}

pub struct PortalHandle {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for PortalHandle {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

/// Spawn the portal worker. `bindings` is a list of
/// (shortcut id, trigger, description).
/// `on_activate` runs on the portal thread with the shortcut id.
pub fn start<F>(
    bindings: Vec<(String, String, String)>,
    events: Sender<PortalEvent>,
    on_activate: F,
) -> PortalHandle
where
    F: Fn(&str) + Send + 'static,
{
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    thread::spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = events.send(PortalEvent::Status(format!("portal runtime: {e}"), false));
                return;
            }
        };
        rt.block_on(async move {
            tokio::select! {
                _ = stop_rx => {}
                _ = run_session(bindings, events, on_activate) => {}
            }
        });
    });
    PortalHandle {
        stop: Some(stop_tx),
    }
}

async fn run_session<F: Fn(&str)>(
    bindings: Vec<(String, String, String)>,
    events: Sender<PortalEvent>,
    on_activate: F,
) {
    use ashpd::desktop::global_shortcuts::{GlobalShortcuts, NewShortcut};

    let setup = async {
        let portal = GlobalShortcuts::new()
            .await
            .map_err(|e| format!("portal connect: {e}"))?;

        let session = portal
            .create_session()
            .await
            .map_err(|e| format!("create session: {e}"))?;

        let shortcuts: Vec<NewShortcut> = bindings
            .iter()
            .map(|(id, trigger, desc)| {
                NewShortcut::new(id.as_str(), desc.as_str()).preferred_trigger(trigger.as_str())
            })
            .collect();

        portal
            .bind_shortcuts(&session, &shortcuts, None)
            .await
            .map_err(|e| format!("bind shortcuts: {e}"))?;

        let stream = portal
            .receive_activated()
            .await
            .map_err(|e| format!("listen: {e}"))?;
        Ok((session, stream))
    };

    let result = tokio::time::timeout(Duration::from_secs(8), setup).await;
    let (session, mut stream) = match result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            let _ = events.send(PortalEvent::Status(e, false));
            return;
        }
        Err(_) => {
            let _ = events.send(PortalEvent::Status("portal: timed out".into(), false));
            return;
        }
    };

    let _ = events.send(PortalEvent::Status(String::new(), true));
    while let Some(activated) = stream.next().await {
        on_activate(activated.shortcut_id());
    }
    let _ = session.close().await;
}
