//! Latest-wins preview renderer.
//!
//! Rendering a 1280x720 PNG takes long enough to be visible on the UI thread,
//! especially while a colour picker is emitting dozens of events per second.
//! This worker owns the Direct2D renderer, keeps only the newest requested
//! configuration, and publishes a small 640x360 PNG for the WebView.
//!
//! Frames are produced on the animation's own deadline rather than a fixed
//! timer: each published frame reports how long until the next one changes, and
//! a request that would only repeat the frame already published is skipped.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tao::event_loop::EventLoopProxy;

use crate::app::AppEvent;
use crate::config::CrosshairConfig;
use crate::render::crosshair::Renderer;

enum PreviewCmd {
    Render(CrosshairConfig),
    Shutdown,
}

/// The newest preview PNG plus the counters that say whether the box keeps up.
/// The render worker and the `/preview` route share it behind one mutex, so a
/// frame is never served while it is being replaced.
#[derive(Default)]
pub struct PreviewCache {
    rev: u64,
    png: Vec<u8>,
    frames: u64,
    skipped: u64,
    serves: u64,
    render_ns: u64,
    render_peak_ns: u64,
}

/// A snapshot of the preview counters, read once per reporting window.
#[derive(Clone, Copy, Default)]
pub struct PreviewCost {
    pub rev: u64,
    pub frames: u64,
    pub skipped: u64,
    pub serves: u64,
    pub render_ns: u64,
    pub render_peak_ns: u64,
}

impl PreviewCache {
    pub fn publish(&mut self, rev: u64, png: Vec<u8>, render_ns: u64) {
        self.rev = rev;
        self.png = png;
        self.frames = self.frames.wrapping_add(1);
        self.render_ns = self.render_ns.saturating_add(render_ns);
        self.render_peak_ns = self.render_peak_ns.max(render_ns);
    }

    /// Counted when a render was requested but the published frame still
    /// matched, so the log can show the encoder was not woken for nothing.
    pub fn note_skip(&mut self) {
        self.skipped = self.skipped.wrapping_add(1);
    }

    /// Serves the newest frame to the WebView, counting the request so a
    /// session's log shows whether the page asks for more than was produced.
    pub fn serve(&mut self) -> Vec<u8> {
        self.serves = self.serves.wrapping_add(1);
        self.png.clone()
    }

    pub fn cost(&self) -> PreviewCost {
        PreviewCost {
            rev: self.rev,
            frames: self.frames,
            skipped: self.skipped,
            serves: self.serves,
            render_ns: self.render_ns,
            render_peak_ns: self.render_peak_ns,
        }
    }
}

pub struct PreviewWorker {
    tx: Sender<PreviewCmd>,
}

impl PreviewWorker {
    pub fn start(proxy: EventLoopProxy<AppEvent>, cache: Arc<Mutex<PreviewCache>>) -> Self {
        let (tx, rx) = mpsc::channel::<PreviewCmd>();
        thread::spawn(move || {
            let mut renderer = match Renderer::new() {
                Ok(renderer) => renderer,
                Err(err) => {
                    crate::logging::log(&format!("preview: renderer init failed: {err}"));
                    return;
                }
            };
            let mut tick = 0u64;
            // What the box currently shows, so an animation frame that is
            // already published is not encoded again.
            let mut shown: Option<(usize, CrosshairConfig)> = None;
            while let Ok(command) = rx.recv() {
                let PreviewCmd::Render(mut config) = command else { break };
                // Drain queued requests so the newest state wins. This is what
                // keeps a colour-picker storm from building an unbounded queue.
                while let Ok(next) = rx.try_recv() {
                    match next {
                        PreviewCmd::Render(next_config) => config = next_config,
                        PreviewCmd::Shutdown => return,
                    }
                }
                let now = crate::render::now_ms();
                renderer.sync_image(&config);
                let frame = renderer.preview_frame(&config, now);
                // A 10ms-per-frame animation would otherwise pin a core; the
                // floor also gives the WebView room to paint and decode.
                let due = renderer.preview_due_ms(&config, now).map(|ms| ms.max(16));
                let repeated = shown
                    .as_ref()
                    .map_or(false, |(last_frame, last_config)| *last_frame == frame && *last_config == config);
                if repeated {
                    if let Ok(mut slot) = cache.lock() {
                        slot.note_skip();
                    }
                    continue;
                }
                let started = Instant::now();
                match renderer.preview_png(&config, now) {
                    Ok(png) => {
                        tick = tick.wrapping_add(1);
                        let render_ns = started.elapsed().as_nanos() as u64;
                        if let Ok(mut slot) = cache.lock() {
                            slot.publish(tick, png, render_ns);
                        }
                        shown = Some((frame, config));
                        let _ = proxy.send_event(AppEvent::PreviewReady {
                            tick,
                            next_ms: due,
                            render_ms: render_ns / 1_000_000,
                        });
                    }
                    Err(err) => crate::logging::log(&format!("preview: render failed: {err}")),
                }
                // Give the event loop and the WebView a chance between frames.
                thread::sleep(Duration::from_millis(2));
            }
        });
        Self { tx }
    }

    pub fn render(&self, config: &CrosshairConfig) {
        let _ = self.tx.send(PreviewCmd::Render(config.clone()));
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(PreviewCmd::Shutdown);
    }
}
