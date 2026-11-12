//! scrcpy v5.0.1 control messages (client to device), as read by the
//! server's `ControlMessageReader.java`. All numbers are big-endian; the
//! first byte is the message type.
//!
//! Only the messages ZenLink sends are here. The device also writes on the
//! control socket (clipboard, acks, UHID output); [`crate::mirror`] drains
//! those so the socket never fills up.

/// Message types (`ControlMessage.TYPE_*`).
pub const TYPE_INJECT_KEYCODE: u8 = 0;
pub const TYPE_INJECT_TEXT: u8 = 1;
pub const TYPE_INJECT_TOUCH_EVENT: u8 = 2;
pub const TYPE_INJECT_SCROLL_EVENT: u8 = 3;
pub const TYPE_BACK_OR_SCREEN_ON: u8 = 4;
pub const TYPE_EXPAND_NOTIFICATION_PANEL: u8 = 5;
pub const TYPE_EXPAND_SETTINGS_PANEL: u8 = 6;
pub const TYPE_COLLAPSE_PANELS: u8 = 7;
pub const TYPE_GET_CLIPBOARD: u8 = 8;
pub const TYPE_SET_CLIPBOARD: u8 = 9;
/// `GET_CLIPBOARD` copy keys (`ControlMessage.COPY_KEY_*`).
pub const COPY_KEY_COPY: u8 = 1;
pub const COPY_KEY_CUT: u8 = 2;

/// Device messages (`DeviceMessage.TYPE_*`).
const DEVICE_CLIPBOARD: u8 = 0;
const DEVICE_ACK_CLIPBOARD: u8 = 1;
const DEVICE_UHID_OUTPUT: u8 = 2;

/// Reads one message the device sends on the control socket: `Ok(Some(text))` for its
/// clipboard, `Ok(None)` for others (acks, UHID output). An unknown type is an error (the
/// stream cannot be followed after it).
pub fn read_device_message(r: &mut impl std::io::Read) -> std::io::Result<Option<String>> {
    let mut t = [0u8; 1];
    r.read_exact(&mut t)?;
    match t[0] {
        DEVICE_CLIPBOARD => {
            let mut n = [0u8; 4];
            r.read_exact(&mut n)?;
            let n = u32::from_be_bytes(n) as usize;
            if n > CLIPBOARD_MAX_BYTES + 14 {
                return Err(std::io::Error::other("clipboard message too long"));
            }
            let mut b = vec![0u8; n];
            r.read_exact(&mut b)?;
            Ok(Some(String::from_utf8_lossy(&b).into_owned()))
        }
        DEVICE_ACK_CLIPBOARD => {
            r.read_exact(&mut [0u8; 8])?;
            Ok(None)
        }
        DEVICE_UHID_OUTPUT => {
            let mut h = [0u8; 4];
            r.read_exact(&mut h)?;
            let n = u16::from_be_bytes([h[2], h[3]]) as usize;
            r.read_exact(&mut vec![0u8; n])?;
            Ok(None)
        }
        other => Err(std::io::Error::other(format!(
            "device message type {other}"
        ))),
    }
}
pub const TYPE_SET_DISPLAY_POWER: u8 = 10;
pub const TYPE_ROTATE_DEVICE: u8 = 11;
pub const TYPE_START_APP: u8 = 16;
/// A fresh config and keyframe from the encoder (scrcpy 3.0+).
pub const TYPE_RESET_VIDEO: u8 = 17;
pub const TYPE_RESIZE_DISPLAY: u8 = 21;

/// `KeyEvent.ACTION_*` and `MotionEvent.ACTION_*`.
pub const ACTION_DOWN: u8 = 0;
pub const ACTION_UP: u8 = 1;
pub const ACTION_MOVE: u8 = 2;

/// A finger that is not the mouse (`POINTER_ID_GENERIC_FINGER`): the
/// server injects it as a touchscreen event.
pub const POINTER_ID_GENERIC_FINGER: i64 = -2;

/// The server refuses longer injected text (`INJECT_TEXT_MAX_LENGTH`).
pub const TEXT_MAX_BYTES: usize = 300;
/// Clipboard text limit (`CLIPBOARD_TEXT_MAX_LENGTH`: 256 KiB minus the header).
pub const CLIPBOARD_MAX_BYTES: usize = (1 << 18) - 14;
/// `START_APP` carries its name with a 1-byte length (`parseString(1)`).
pub const APP_NAME_MAX_BYTES: usize = 255;

/// Android key codes ZenLink uses (`KeyEvent.KEYCODE_*`).
pub mod keycode {
    pub const HOME: i32 = 3;
    pub const BACK: i32 = 4;
    pub const DPAD_UP: i32 = 19;
    pub const DPAD_DOWN: i32 = 20;
    pub const DPAD_LEFT: i32 = 21;
    pub const DPAD_RIGHT: i32 = 22;
    pub const VOLUME_UP: i32 = 24;
    pub const VOLUME_DOWN: i32 = 25;
    pub const POWER: i32 = 26;
    /// KEYCODE_A; B..Z follow in order.
    pub const A: i32 = 29;
    pub const TAB: i32 = 61;
    pub const SPACE: i32 = 62;
    pub const ENTER: i32 = 66;
    pub const DEL: i32 = 67;
    pub const PAGE_UP: i32 = 92;
    pub const PAGE_DOWN: i32 = 93;
    pub const ESCAPE: i32 = 111;
    pub const FORWARD_DEL: i32 = 112;
    pub const MOVE_HOME: i32 = 122;
    pub const MOVE_END: i32 = 123;
    pub const APP_SWITCH: i32 = 187;
}

/// `KeyEvent.META_*` flags.
pub mod meta {
    pub const SHIFT_ON: i32 = 0x1;
    pub const ALT_ON: i32 = 0x2;
    pub const CTRL_ON: i32 = 0x1000;
}

/// Where a pointer event happened, in pixels of the video frame the
/// device is sending. The server ignores events made for another size
/// (for example just after a rotation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub x: i32,
    pub y: i32,
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Control {
    Key {
        action: u8,
        keycode: i32,
        repeat: i32,
        meta: i32,
    },
    /// UTF-8, cut to [`TEXT_MAX_BYTES`] on a character boundary.
    Text(String),
    Touch {
        action: u8,
        pointer: i64,
        pos: Position,
        /// 0.0..=1.0
        pressure: f32,
        action_button: i32,
        buttons: i32,
    },
    /// Scroll amounts in wheel notches (positive = up / right), -16..=16.
    Scroll {
        pos: Position,
        h: f32,
        v: f32,
        buttons: i32,
    },
    /// BACK, or turns the screen on when it is off.
    BackOrScreenOn {
        action: u8,
    },
    ExpandNotifications,
    ExpandSettings,
    CollapsePanels,
    /// Asks for the device clipboard (it answers with [`read_device_message`]); `copy_key`
    /// [`COPY_KEY_COPY`] / [`COPY_KEY_CUT`] presses COPY / CUT on the device first.
    GetClipboard {
        copy_key: u8,
    },
    /// Sets the device clipboard; `paste` also pastes it (like Ctrl+V).
    SetClipboard {
        sequence: i64,
        paste: bool,
        text: String,
    },
    /// Turns the device screen off (false) or on while mirroring goes on.
    SetDisplayPower(bool),
    RotateDevice,
    /// Starts an app by package name on the mirrored display (the new
    /// virtual display when there is one). A `+` prefix force-stops it first.
    StartApp(String),
    /// Restarts the encoder: a new config packet and a keyframe follow.
    ResetVideo,
    /// New size for a flex virtual display (`flex_display=true`), in pixels.
    ResizeDisplay {
        width: u16,
        height: u16,
    },
}

/// `sc_float_to_u16fp`: 1.0 maps to 0xffff.
fn u16fp(f: f32) -> u16 {
    let f = f.clamp(0.0, 1.0);
    if f >= 1.0 {
        0xffff
    } else {
        (f * 65536.0) as u16
    }
}

/// `sc_float_to_i16fp` for -1.0..=1.0.
fn i16fp(f: f32) -> i16 {
    let i = (f.clamp(-1.0, 1.0) * 32768.0) as i32;
    i.min(0x7fff) as i16
}

fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn put_position(w: &mut Vec<u8>, p: &Position) {
    w.extend_from_slice(&p.x.to_be_bytes());
    w.extend_from_slice(&p.y.to_be_bytes());
    w.extend_from_slice(&p.width.to_be_bytes());
    w.extend_from_slice(&p.height.to_be_bytes());
}

fn put_string(w: &mut Vec<u8>, s: &str) {
    w.extend_from_slice(&(s.len() as u32).to_be_bytes());
    w.extend_from_slice(s.as_bytes());
}

impl Control {
    /// The bytes the server reads for this message.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Vec::with_capacity(32);
        match self {
            Control::Key {
                action,
                keycode,
                repeat,
                meta,
            } => {
                w.push(TYPE_INJECT_KEYCODE);
                w.push(*action);
                w.extend_from_slice(&keycode.to_be_bytes());
                w.extend_from_slice(&repeat.to_be_bytes());
                w.extend_from_slice(&meta.to_be_bytes());
            }
            Control::Text(t) => {
                w.push(TYPE_INJECT_TEXT);
                put_string(&mut w, cut(t, TEXT_MAX_BYTES));
            }
            Control::Touch {
                action,
                pointer,
                pos,
                pressure,
                action_button,
                buttons,
            } => {
                w.push(TYPE_INJECT_TOUCH_EVENT);
                w.push(*action);
                w.extend_from_slice(&pointer.to_be_bytes());
                put_position(&mut w, pos);
                w.extend_from_slice(&u16fp(*pressure).to_be_bytes());
                w.extend_from_slice(&action_button.to_be_bytes());
                w.extend_from_slice(&buttons.to_be_bytes());
            }
            Control::Scroll { pos, h, v, buttons } => {
                w.push(TYPE_INJECT_SCROLL_EVENT);
                put_position(&mut w, pos);
                w.extend_from_slice(&i16fp(h / 16.0).to_be_bytes());
                w.extend_from_slice(&i16fp(v / 16.0).to_be_bytes());
                w.extend_from_slice(&buttons.to_be_bytes());
            }
            Control::BackOrScreenOn { action } => {
                w.push(TYPE_BACK_OR_SCREEN_ON);
                w.push(*action);
            }
            Control::ExpandNotifications => w.push(TYPE_EXPAND_NOTIFICATION_PANEL),
            Control::ExpandSettings => w.push(TYPE_EXPAND_SETTINGS_PANEL),
            Control::CollapsePanels => w.push(TYPE_COLLAPSE_PANELS),
            Control::GetClipboard { copy_key } => {
                w.push(TYPE_GET_CLIPBOARD);
                w.push(*copy_key);
            }
            Control::SetClipboard {
                sequence,
                paste,
                text,
            } => {
                w.push(TYPE_SET_CLIPBOARD);
                w.extend_from_slice(&sequence.to_be_bytes());
                w.push(u8::from(*paste));
                put_string(&mut w, cut(text, CLIPBOARD_MAX_BYTES));
            }
            Control::SetDisplayPower(on) => {
                w.push(TYPE_SET_DISPLAY_POWER);
                w.push(u8::from(*on));
            }
            Control::RotateDevice => w.push(TYPE_ROTATE_DEVICE),
            Control::ResetVideo => w.push(TYPE_RESET_VIDEO),
            Control::StartApp(name) => {
                let name = cut(name, APP_NAME_MAX_BYTES);
                w.push(TYPE_START_APP);
                w.push(name.len() as u8);
                w.extend_from_slice(name.as_bytes());
            }
            Control::ResizeDisplay { width, height } => {
                w.push(TYPE_RESIZE_DISPLAY);
                w.extend_from_slice(&width.to_be_bytes());
                w.extend_from_slice(&height.to_be_bytes());
            }
        }
        w
    }

    /// Key press and release.
    pub fn key_tap(keycode: i32, meta: i32) -> [Control; 2] {
        [ACTION_DOWN, ACTION_UP].map(|action| Control::Key {
            action,
            keycode,
            repeat: 0,
            meta,
        })
    }
}

/// A point on the picture (0..1, frame orientation) in frame pixels.
pub fn position(u: f32, v: f32, width: u16, height: u16) -> Position {
    let px = |t: f32, n: u16| {
        ((t.clamp(0.0, 1.0) * n as f32) as i32)
            .min(i32::from(n) - 1)
            .max(0)
    };
    Position {
        x: px(u, width),
        y: px(v, height),
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POS: Position = Position {
        x: 260,
        y: 1026,
        width: 1080,
        height: 1920,
    };

    // Byte layouts from ControlMessageReader.java (v5.0.1) and the
    // client's control_msg.c serializer.

    #[test]
    fn keycode_layout() {
        let m = Control::Key {
            action: ACTION_UP,
            keycode: keycode::ENTER,
            repeat: 5,
            meta: meta::SHIFT_ON | meta::CTRL_ON,
        };
        assert_eq!(
            m.encode(),
            [0, 1, 0, 0, 0, 0x42, 0, 0, 0, 5, 0, 0, 0x10, 0x01]
        );
    }

    #[test]
    fn text_layout_and_limit() {
        assert_eq!(
            Control::Text("hé".into()).encode(),
            [1, 0, 0, 0, 3, b'h', 0xc3, 0xa9]
        );
        let long = "é".repeat(200); // 400 bytes
        let b = Control::Text(long).encode();
        assert_eq!(&b[1..5], &300u32.to_be_bytes());
        assert_eq!(b.len(), 5 + 300);
    }

    #[test]
    fn touch_layout() {
        let m = Control::Touch {
            action: ACTION_DOWN,
            pointer: POINTER_ID_GENERIC_FINGER,
            pos: POS,
            pressure: 1.0,
            action_button: 0,
            buttons: 0,
        };
        let b = m.encode();
        assert_eq!(b.len(), 32, "1 + 1 + 8 + 12 + 2 + 4 + 4");
        assert_eq!(&b[..2], &[2, 0]);
        assert_eq!(&b[2..10], &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xfe]);
        assert_eq!(&b[10..22], &[0, 0, 1, 4, 0, 0, 4, 2, 4, 0x38, 7, 0x80]);
        assert_eq!(&b[22..24], &[0xff, 0xff], "pressure 1.0");
        assert_eq!(&b[24..32], &[0; 8]);
    }

    #[test]
    fn scroll_layout() {
        let b = Control::Scroll {
            pos: POS,
            h: 16.0,
            v: -1.0,
            buttons: 0,
        }
        .encode();
        assert_eq!(b.len(), 21, "1 + 12 + 2 + 2 + 4");
        assert_eq!(b[0], 3);
        assert_eq!(&b[13..15], &[0x7f, 0xff], "16 notches clamps to 1.0");
        assert_eq!(&b[15..17], &(-2048i16).to_be_bytes(), "-1 notch = -1/16");
    }

    #[test]
    fn small_messages() {
        assert_eq!(
            Control::BackOrScreenOn {
                action: ACTION_DOWN
            }
            .encode(),
            [4, 0]
        );
        assert_eq!(Control::ExpandNotifications.encode(), [5]);
        assert_eq!(Control::ExpandSettings.encode(), [6]);
        assert_eq!(Control::CollapsePanels.encode(), [7]);
        assert_eq!(Control::SetDisplayPower(false).encode(), [10, 0]);
        assert_eq!(Control::RotateDevice.encode(), [11]);
        let tap = Control::key_tap(keycode::HOME, 0);
        assert_eq!(tap[0].encode()[1], ACTION_DOWN);
        assert_eq!(tap[1].encode()[1], ACTION_UP);
    }

    #[test]
    fn start_app_and_resize_layout() {
        assert_eq!(
            Control::StartApp("org.videolan.vlc".into()).encode(),
            [&[16u8, 16][..], b"org.videolan.vlc"].concat()
        );
        let long = Control::StartApp("a.".repeat(200)).encode();
        assert_eq!(long[1], 255, "1-byte length");
        assert_eq!(long.len(), 2 + 255);
        assert_eq!(
            Control::ResizeDisplay {
                width: 1280,
                height: 720
            }
            .encode(),
            [21, 0x05, 0x00, 0x02, 0xd0]
        );
    }

    #[test]
    fn clipboard_layout() {
        let b = Control::SetClipboard {
            sequence: 7,
            paste: true,
            text: "ok".into(),
        }
        .encode();
        assert_eq!(b, [9, 0, 0, 0, 0, 0, 0, 0, 7, 1, 0, 0, 0, 2, b'o', b'k']);
        assert_eq!(
            Control::GetClipboard {
                copy_key: COPY_KEY_COPY
            }
            .encode(),
            [8, 1]
        );
    }

    #[test]
    fn device_messages() {
        let mut s: &[u8] = &[
            1, 0, 0, 0, 0, 0, 0, 0, 7, // ack
            2, 0, 1, 0, 2, 0xaa, 0xbb, // UHID output, 2 bytes
            0, 0, 0, 0, 2, b'h', b'i', // clipboard "hi"
            9,    // unknown
        ];
        assert_eq!(read_device_message(&mut s).unwrap(), None);
        assert_eq!(read_device_message(&mut s).unwrap(), None);
        assert_eq!(read_device_message(&mut s).unwrap().as_deref(), Some("hi"));
        assert!(read_device_message(&mut s).is_err());
    }

    #[test]
    fn position_stays_inside_the_frame() {
        assert_eq!(
            position(0.0, 0.0, 1080, 1920),
            Position {
                x: 0,
                y: 0,
                width: 1080,
                height: 1920
            }
        );
        let p = position(1.0, 0.5, 1080, 1920);
        assert_eq!((p.x, p.y), (1079, 960));
        let p = position(-3.0, 9.0, 100, 50);
        assert_eq!((p.x, p.y), (0, 49));
    }
}
