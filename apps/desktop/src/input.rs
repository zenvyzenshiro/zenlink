//! Input worker: sends touches, strokes and buttons in order, off the UI thread, to the Rust
//! Apple engine in Apple Driver mode. Consecutive moves are coalesced
//! (only the newest is sent).

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Duration;

use zenlink_apple::hid;
use zenlink_core::api::{device_point, TouchKind};

use crate::apple;

/// Input goes to the Rust Apple engine in Apple Driver mode; ZenLink Studio has no control.
struct Out;

impl Out {
    fn send(&self, c: hid::Cmd) {
        if apple::in_use() {
            apple::engine().send(c);
        }
    }

    fn touch(&self, kind: TouchKind, x: u16, y: u16) {
        let contact = matches!(kind, TouchKind::Contact);
        self.send(hid::Cmd::Touch { contact, x, y });
    }

    fn home(&self) {
        self.send(hid::Cmd::Button("home"));
    }

    fn action(&self, name: &str, text: &str) {
        self.send(hid::Cmd::Action {
            name: name.into(),
            text: text.into(),
        });
    }

    fn tap(&self, usages: Vec<u8>) {
        self.send(hid::Cmd::Keys(usages));
        self.send(hid::Cmd::Keys(Vec::new()));
    }
}

#[derive(Clone, Debug)]
pub enum Cmd {
    Down(f32, f32),
    Move(f32, f32),
    Up(f32, f32),
    /// u0, v0, u1, v1, steps, ms per step
    Stroke(f32, f32, f32, f32, u32, u64),
    Home(u32),
    Action(&'static str),
    /// Apple hardware keyboard: one key tap (HID usages held, then none).
    Keys(Vec<u8>),
    /// Apple `paste-pc`: type this text on the device.
    TypeText(String),
    /// Two pinch fingers ([`crate::pinch`]).
    Fingers(crate::pinch::Phase, crate::pinch::Fingers),
}

#[derive(Clone)]
pub struct Input {
    tx: Sender<Cmd>,
    /// Rotation the picture is shown with (degrees), for touch mapping.
    pub rotation: Arc<AtomicU32>,
}

impl Input {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        let rotation = Arc::new(AtomicU32::new(0));
        let rot = rotation.clone();
        std::thread::Builder::new()
            .name("zenlink-input".into())
            .spawn(move || worker(rx, rot))
            .expect("input thread");
        Self { tx, rotation }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }
}

fn worker(rx: Receiver<Cmd>, rot: Arc<AtomicU32>) {
    let api = Out;
    let pt = |u: f32, v: f32| device_point(u, v, rot.load(Relaxed));
    let mut next: Option<Cmd> = None;
    loop {
        let cmd = match next.take() {
            Some(c) => c,
            None => match rx.recv() {
                Ok(c) => c,
                Err(_) => return,
            },
        };
        match cmd {
            Cmd::Move(mut u, mut v) => {
                // Skip to the newest queued move.
                while let Ok(c) = rx.try_recv() {
                    match c {
                        Cmd::Move(a, b) => (u, v) = (a, b),
                        other => {
                            next = Some(other);
                            break;
                        }
                    }
                }
                let (x, y) = pt(u, v);
                api.touch(TouchKind::Contact, x, y);
            }
            Cmd::Down(u, v) => {
                let (x, y) = pt(u, v);
                api.touch(TouchKind::Contact, x, y);
            }
            Cmd::Up(u, v) => {
                let (x, y) = pt(u, v);
                api.touch(TouchKind::Release, x, y);
            }
            Cmd::Stroke(u0, v0, u1, v1, steps, dt) => {
                let (x, y) = pt(u0, v0);
                api.touch(TouchKind::Contact, x, y);
                for i in 1..=steps {
                    std::thread::sleep(Duration::from_millis(dt));
                    let k = i as f32 / steps as f32;
                    let (x, y) = pt(u0 + (u1 - u0) * k, v0 + (v1 - v0) * k);
                    api.touch(TouchKind::Contact, x, y);
                }
                let (x, y) = pt(u1, v1);
                api.touch(TouchKind::Release, x, y);
            }
            Cmd::Home(n) => {
                for i in 0..n {
                    if i > 0 {
                        std::thread::sleep(Duration::from_millis(120));
                    }
                    api.home();
                }
            }
            Cmd::Action(a) => api.action(a, ""),
            Cmd::Keys(usages) => api.tap(usages),
            Cmd::TypeText(text) => api.action("paste-pc", &text),
            Cmd::Fingers(phase, f) => api.send(hid::Cmd::Fingers {
                touching: phase != crate::pinch::Phase::Up,
                points: f.map(|(u, v)| pt(u, v)),
            }),
        }
    }
}
