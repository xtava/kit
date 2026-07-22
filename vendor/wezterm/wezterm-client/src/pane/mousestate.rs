use crate::client::Client;
use anyhow::{anyhow, Context};
use codec::*;
use mux::pane::PaneId;
use mux::{ClientPaneTaskKind, Mux};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use wezterm_term::{MouseButton, MouseEvent, MouseEventKind};

const MAX_QUEUED_MOUSE_EVENTS: usize = 64;

pub struct MouseState {
    draining: bool,
    queue: VecDeque<MouseEvent>,
    client: Client,
    local_pane_id: PaneId,
    remote_pane_id: PaneId,
}

struct DrainReset(Arc<Mutex<MouseState>>);

impl Drop for DrainReset {
    fn drop(&mut self) {
        self.0.lock().draining = false;
    }
}

impl MouseState {
    pub fn new(local_pane_id: PaneId, remote_pane_id: PaneId, client: Client) -> Self {
        Self {
            local_pane_id,
            remote_pane_id,
            client,
            draining: false,
            queue: VecDeque::new(),
        }
    }

    fn append(&mut self, event: MouseEvent) -> anyhow::Result<()> {
        if let Some(last) = self.queue.back_mut() {
            if last.modifiers == event.modifiers {
                if last.kind == MouseEventKind::Move
                    && event.kind == MouseEventKind::Move
                    && last.button == event.button
                {
                    // Collapse any interim moves and just buffer up the
                    // last of them.
                    *last = event;
                    return Ok(());
                }

                // Similarly, for repeated wheel scrolls, add up the deltas
                // rather than swamping the queue.
                match (&last.button, &event.button) {
                    (MouseButton::WheelUp(a), MouseButton::WheelUp(b)) => {
                        last.button = MouseButton::WheelUp(a + b);
                        return Ok(());
                    }
                    (MouseButton::WheelDown(a), MouseButton::WheelDown(b)) => {
                        last.button = MouseButton::WheelDown(a + b);
                        return Ok(());
                    }
                    (MouseButton::WheelLeft(a), MouseButton::WheelLeft(b)) => {
                        last.button = MouseButton::WheelLeft(a + b);
                        return Ok(());
                    }
                    (MouseButton::WheelRight(a), MouseButton::WheelRight(b)) => {
                        last.button = MouseButton::WheelRight(a + b);
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

        if self.queue.len() >= MAX_QUEUED_MOUSE_EVENTS {
            return Err(anyhow!(
                "mouse event queue for pane {} is saturated at {} events",
                self.local_pane_id,
                MAX_QUEUED_MOUSE_EVENTS
            ));
        }
        self.queue.push_back(event);
        log::trace!("MouseEvent {}: queued", self.queue.len());
        Ok(())
    }

    async fn drain<F>(
        state: Arc<Mutex<Self>>,
        _reset: DrainReset,
        first_request: F,
    ) -> anyhow::Result<()>
    where
        F: Future<Output = anyhow::Result<AdmittedRpcResponse<UnitResponse>>> + Send + 'static,
    {
        if let Err(err) = first_request.await.context("sending queued mouse event") {
            state.lock().draining = false;
            return Err(err);
        }

        loop {
            let (client, remote_pane_id, event) = {
                let mut mouse = state.lock();
                match mouse.queue.pop_front() {
                    Some(event) => (mouse.client.clone(), mouse.remote_pane_id, event),
                    None => {
                        mouse.draining = false;
                        return Ok(());
                    }
                }
            };

            if let Err(err) = client
                .mouse_event(SendMouseEvent {
                    pane_id: remote_pane_id,
                    event,
                })
                .await
                .context("sending queued mouse event")
            {
                state.lock().draining = false;
                return Err(err);
            }
        }
    }

    pub fn enqueue(state: &Arc<Mutex<Self>>, event: MouseEvent) -> anyhow::Result<bool> {
        let (local_pane_id, first_request) = {
            let mut mouse = state.lock();
            mouse.append(event)?;
            if mouse.draining {
                return Ok(false);
            }
            mouse.draining = true;
            let event = mouse
                .queue
                .pop_front()
                .expect("a newly started mouse drain has one queued event");
            let request = mouse.client.mouse_event(SendMouseEvent {
                pane_id: mouse.remote_pane_id,
                event,
            });
            (mouse.local_pane_id, request)
        };

        let drain_state = Arc::clone(state);
        let reset = DrainReset(Arc::clone(state));
        Mux::get()
            .try_spawn_client_pane_task(
                local_pane_id,
                ClientPaneTaskKind::Request,
                Self::drain(drain_state, reset, first_request),
            )
            .context("scheduling mouse event drain")?;
        Ok(true)
    }
}
