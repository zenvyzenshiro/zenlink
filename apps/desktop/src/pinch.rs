//! Pinch zoom (R59): Ctrl + mouse wheel, or a touchpad pinch (Windows sends it as Ctrl + wheel),
//! becomes a two-finger pinch on the device (Android, and iPhone / iPad in Apple Driver mode).
//! The two fingers go down at the pointer, spread or close a little per wheel notch, and lift
//! when the wheel has been quiet for a moment, so one long pinch is one gesture on the device.

use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Phase {
    Down,
    Move,
    Up,
}

/// The two fingers, u, v on the picture as the user sees it (0..1).
pub type Fingers = [(f32, f32); 2];

/// Where a gesture goes; picked by the first wheel event of the gesture.
pub type Sink = Arc<dyn Fn(Phase, Fingers) + Send + Sync>;

/// Wheel quiet this long: the fingers lift.
const LIFT_AFTER: Duration = Duration::from_millis(300);
/// Moves per notch, 8 ms apart (smooth enough for the device's gesture detector).
const STEPS: u32 = 4;
const STEP: Duration = Duration::from_millis(8);

/// Finger distance from the centre (picture fraction) at the start: close for zoom in, wide
/// for zoom out, so both directions have room to move.
fn start_radius(d: f32) -> f32 {
    if d >= 0.0 {
        0.06
    } else {
        0.30
    }
}

/// Distance after `d` notches (positive = zoom in = fingers apart).
fn next_radius(r: f32, d: f32) -> f32 {
    (r * 1.25f32.powf(d)).clamp(0.02, 0.45)
}

/// The fingers left and right of `c`, kept on the picture.
fn fingers(c: (f32, f32), r: f32) -> Fingers {
    [
        ((c.0 - r).clamp(0.0, 1.0), c.1),
        ((c.0 + r).clamp(0.0, 1.0), c.1),
    ]
}

type Msg = (f32, f32, f32, Sink);

/// One pinch step at `u, v` (`d` notches); the gesture goes to `sink`.
pub fn zoom(u: f32, v: f32, d: f32, sink: Sink) {
    static TX: OnceLock<Option<Sender<Msg>>> = OnceLock::new();
    let tx = TX.get_or_init(|| {
        let (tx, rx) = channel::<Msg>();
        std::thread::Builder::new()
            .name("zenlink-pinch".into())
            .spawn(move || {
                while let Ok((u, v, mut d, out)) = rx.recv() {
                    let c = (u, v);
                    let mut r = start_radius(d);
                    out(Phase::Down, fingers(c, r));
                    loop {
                        let to = next_radius(r, d);
                        for i in 1..=STEPS {
                            std::thread::sleep(STEP);
                            out(
                                Phase::Move,
                                fingers(c, r + (to - r) * i as f32 / STEPS as f32),
                            );
                        }
                        r = to;
                        match rx.recv_timeout(LIFT_AFTER) {
                            Ok((_, _, nd, _)) => d = nd,
                            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {
                                break
                            }
                        }
                    }
                    out(Phase::Up, fingers(c, r));
                }
            })
            .ok()
            .map(|_| tx)
    });
    if let Some(tx) = tx {
        let _ = tx.send((u, v, d, sink));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radius_follows_the_wheel() {
        let r = start_radius(1.0);
        assert!(next_radius(r, 1.0) > r, "zoom in spreads the fingers");
        let r = start_radius(-1.0);
        assert!(next_radius(r, -1.0) < r, "zoom out closes them");
        assert_eq!(next_radius(0.44, 10.0), 0.45);
        assert_eq!(next_radius(0.03, -10.0), 0.02);
    }

    #[test]
    fn fingers_stay_on_the_picture() {
        let f = fingers((0.02, 0.5), 0.1);
        assert_eq!(f[0], (0.0, 0.5));
        assert!((f[1].0 - 0.12).abs() < 1e-6);
    }
}
