//! Touch, hardware buttons and the keyboard over CoreDevice HID, plus ZenLink's keyboard
//! shortcuts on top. The events match pymobiledevice3's screen-stream server that ZenLink
//! 2.5.x used: touches are mainTouchscreen reports and keys set mainKeyboard's pressed-key set
//! (UniversalHID); buttons are Indigo button events held for a moment.

use std::time::Duration;

use idevice::{
    core_device::{
        ButtonState, IndigoHidClient, KeyboardUsage, MainKeyboardService, TouchscreenContact,
        UniversalHidServiceClient, TOUCHSCREEN_STATE_CONTACT, TOUCHSCREEN_STATE_RELEASE,
    },
    rsd::RsdHandshake,
    tcp::handle::AdapterHandle,
    ReadWrite, RsdService as _,
};

#[derive(Debug)]
pub enum Cmd {
    /// x, y in 0..65535 of the portrait screen.
    Touch {
        contact: bool,
        x: u16,
        y: u16,
    },
    /// Two fingers at once (pinch), x, y as for `Touch`; `touching: false` lifts both.
    Fingers {
        touching: bool,
        points: [(u16, u16); 2],
    },
    Button(&'static str),
    /// HID keyboard usages held down; empty releases them all.
    Keys(Vec<u8>),
    /// A shortcut (see `Hid::action`). `text`: the text for `paste-pc`, the passcode for `wake`.
    Action {
        name: String,
        text: String,
    },
}

const CMD: u8 = 0xE3;
const SHIFT: u8 = 0xE1;
const ENTER: u8 = 0x28;
const SPACE: u8 = 0x2C;
pub const MAX_TEXT: usize = 4000;
const MAX_PASSCODE: usize = 32;

/// (usage page, usage, hold time in ms), as pymobiledevice3 and ZenLink 2.5.x sent them.
fn button(name: &str) -> Option<(u64, u64, u64)> {
    Some(match name {
        "home" => (0x0C, 0x40, 50),
        "lock" => (0x0C, 0x30, 500),
        "eject" => (0x0C, 0xB8, 50), // shows / hides the on-screen keyboard
        "brightness-down" => (0x0C, 0x70, 25),
        "brightness-up" => (0x0C, 0x6F, 25),
        "volume-up" => (0x0C, 0xE9, 50),
        "volume-down" => (0x0C, 0xEA, 50),
        _ => return None,
    })
}

fn cmd_key(action: &str) -> Option<u8> {
    Some(match action {
        "copy" => 0x06,
        "paste" => 0x19,
        "cut" => 0x1B,
        "select-all" => 0x04,
        "undo" => 0x1D,
        _ => return None,
    })
}

const PLAIN: &str = "1234567890-=[]\\;',./`";
const SHIFTED: &str = "!@#$%^&*()_+{}|:\"<>?~";
const PLAIN_CODES: [u8; 21] = [
    0x1E, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x2D, 0x2E, 0x2F, 0x30, 0x31, 0x33,
    0x34, 0x36, 0x37, 0x38, 0x35,
];

/// (HID usage, needs shift) for one character on a US layout.
pub fn char_usage(ch: char) -> Option<(u8, bool)> {
    match ch {
        'a'..='z' => Some((0x04 + (ch as u8 - b'a'), false)),
        'A'..='Z' => Some((0x04 + (ch as u8 - b'A'), true)),
        ' ' => Some((SPACE, false)),
        '\n' => Some((ENTER, false)),
        '\t' => Some((0x2B, false)),
        _ => {
            if let Some(i) = PLAIN.find(ch) {
                Some((PLAIN_CODES[i], false))
            } else {
                SHIFTED.find(ch).map(|i| (PLAIN_CODES[i], true))
            }
        }
    }
}

fn report(ch: char) -> Option<Vec<u8>> {
    char_usage(ch).map(|(u, shift)| if shift { vec![SHIFT, u] } else { vec![u] })
}

/// The passcode as saved: up to 32 characters typeable on a US keyboard.
pub fn clean_passcode(raw: &str) -> String {
    raw.chars()
        .filter(|c| !"\r\n\t".contains(*c) && char_usage(*c).is_some())
        .take(MAX_PASSCODE)
        .collect()
}

/// Key reports that type `code` on the lock screen: up to 32 typeable characters; a 4- or
/// 6-digit code unlocks by itself, any other needs Return.
pub fn passcode_keys(raw: &str) -> Vec<Vec<u8>> {
    let code = clean_passcode(raw);
    let mut out: Vec<Vec<u8>> = code.chars().filter_map(report).collect();
    let digits = code.chars().all(|c| c.is_ascii_digit()) && matches!(code.len(), 4 | 6);
    if !code.is_empty() && !digits {
        out.push(vec![ENTER]);
    }
    out
}

type Uni = UniversalHidServiceClient<Box<dyn ReadWrite>>;
type Indigo = IndigoHidClient<Box<dyn ReadWrite>>;

/// The HID services of one link, opened on first use and reopened after an error.
pub struct Hid {
    adapter: AdapterHandle,
    rsd: RsdHandshake,
    /// iPad: `wake` uses Home and a key instead of a swipe.
    ipad: bool,
    uni: Option<Uni>,
    keyboard: Option<MainKeyboardService>,
    indigo: Option<Indigo>,
    /// `stealth` dimmed the device's screen.
    dimmed: bool,
}

async fn sleep(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

impl Hid {
    pub fn new(adapter: AdapterHandle, rsd: RsdHandshake, ipad: bool) -> Self {
        Self {
            adapter,
            rsd,
            ipad,
            uni: None,
            keyboard: None,
            indigo: None,
            dimmed: false,
        }
    }

    /// Runs one command. `Ok` carries a note for the user (may be empty).
    pub async fn run(&mut self, cmd: Cmd) -> Result<String, String> {
        let r = match cmd {
            Cmd::Touch { contact, x, y } => self.touch(contact, x, y).await.map(|_| String::new()),
            Cmd::Fingers { touching, points } => {
                let contacts = [0u8, 1].map(|i| TouchscreenContact {
                    identity: i,
                    touching,
                    x: points[i as usize].0,
                    y: points[i as usize].1,
                });
                let r = self.uni().await;
                match r {
                    Ok(u) => u
                        .send_multitouch(&contacts, None)
                        .await
                        .map(|_| String::new())
                        .map_err(|e| format!("pinch: {e:?}")),
                    Err(e) => Err(e),
                }
            }
            Cmd::Button(name) => self.button(name).await.map(|_| String::new()),
            Cmd::Keys(usages) => self.keys(&usages).await.map(|_| String::new()),
            Cmd::Action { name, text } => self.action(&name, &text).await,
        };
        if r.is_err() {
            // The services may have closed (device locked long, tunnel hiccup): reopen next time.
            self.uni = None;
            self.keyboard = None;
            self.indigo = None;
        }
        r
    }

    async fn uni(&mut self) -> Result<&mut Uni, String> {
        if self.uni.is_none() {
            let c = Uni::connect_rsd(&mut self.adapter, &mut self.rsd)
                .await
                .map_err(|e| format!("touch service: {e:?}"))?;
            self.uni = Some(c);
        }
        Ok(self.uni.as_mut().expect("set above"))
    }

    async fn touch(&mut self, contact: bool, x: u16, y: u16) -> Result<(), String> {
        let state = if contact {
            TOUCHSCREEN_STATE_CONTACT
        } else {
            TOUCHSCREEN_STATE_RELEASE
        };
        self.uni()
            .await?
            .send_touchscreen(state, x, y, None)
            .await
            .map_err(|e| format!("touch: {e:?}"))
    }

    async fn keys(&mut self, usages: &[u8]) -> Result<(), String> {
        self.uni().await?;
        let uni = self.uni.as_mut().expect("opened above");
        if self.keyboard.is_none() {
            let kb = uni
                .create_main_keyboard()
                .await
                .map_err(|e| format!("keyboard: {e}"))?;
            self.keyboard = Some(kb);
        }
        let kb = self.keyboard.as_mut().expect("set above");
        let pressed = usages
            .iter()
            .filter_map(|u| KeyboardUsage::new(u16::from(*u)).ok());
        uni.set_main_keyboard_usages(kb, pressed)
            .await
            .map_err(|e| format!("key: {e}"))
    }

    async fn button(&mut self, name: &str) -> Result<(), String> {
        let (page, code, hold) = button(name).ok_or_else(|| format!("unknown button {name}"))?;
        if self.indigo.is_none() {
            let c = Indigo::connect_rsd(&mut self.adapter, &mut self.rsd)
                .await
                .map_err(|e| format!("button service: {e:?}"))?;
            self.indigo = Some(c);
        }
        let b = self.indigo.as_mut().expect("set above");
        let err = |e| format!("button: {e:?}");
        b.send_button(page, code, ButtonState::Down)
            .await
            .map_err(err)?;
        sleep(hold).await;
        b.send_button(page, code, ButtonState::Up)
            .await
            .map_err(err)
    }

    async fn tap(&mut self, report: &[u8], down_ms: u64, up_ms: u64) -> Result<(), String> {
        self.keys(report).await?;
        sleep(down_ms).await;
        self.keys(&[]).await?;
        sleep(up_ms).await;
        Ok(())
    }

    /// ZenLink's shortcuts:
    /// copy, paste, cut, select-all, undo: Cmd + C / V / X / A / Z;
    /// paste-pc: type `text` (US layout); toggle-keyboard: the on-screen keyboard (Eject);
    /// lock: restore brightness if dimmed, then sleep; wake: wake, open the passcode prompt and
    /// type `text` (the saved passcode); stealth: dim the device's own screen to 0% / restore
    /// (the mirrored picture stays full quality).
    pub async fn action(&mut self, name: &str, text: &str) -> Result<String, String> {
        if let Some(k) = cmd_key(name) {
            self.keys(&[]).await?;
            self.keys(&[CMD]).await?;
            sleep(30).await;
            self.keys(&[CMD, k]).await?;
            sleep(50).await;
            self.keys(&[CMD]).await?;
            self.keys(&[]).await?;
            return Ok(String::new());
        }
        match name {
            "toggle-keyboard" => self.button("eject").await.map(|_| String::new()),
            "stealth" => {
                // 16 steps down reach 0%; restoring sends 4 steps up (~25%), and only when
                // ZenLink dimmed it, so the screen never jumps to full.
                let (step, n) = if self.dimmed {
                    ("brightness-up", 4)
                } else {
                    ("brightness-down", 16)
                };
                for _ in 0..n {
                    self.button(step).await?;
                    sleep(30).await;
                }
                self.dimmed = !self.dimmed;
                Ok(if self.dimmed { "dimmed" } else { "restored" }.into())
            }
            "lock" => {
                if self.dimmed {
                    Box::pin(self.action("stealth", "")).await?;
                }
                self.button("lock").await.map(|_| String::new())
            }
            "wake" => {
                if self.ipad {
                    // Tested on iPadOS 27 (Touch ID iPad, landscape): Home turns the screen on;
                    // the first key on the lock screen opens the passcode prompt (it is not
                    // typed). No touches, so the rotation does not matter.
                    self.button("home").await?;
                    sleep(1000).await;
                    self.tap(&[SPACE], 40, 800).await?;
                } else {
                    // iPhone: the lock screen is always portrait, so swipe up.
                    self.button("home").await?;
                    sleep(450).await;
                    self.touch(true, 32768, 58000).await?;
                    for i in 1..=8u32 {
                        sleep(25).await;
                        self.touch(true, 32768, (58000 - 46000 * i / 8) as u16)
                            .await?;
                    }
                    self.touch(false, 32768, 12000).await?;
                    sleep(500).await;
                }
                let keys = passcode_keys(text);
                for r in &keys {
                    self.tap(r, 40, 60).await?;
                }
                Ok(if keys.is_empty() { "woken" } else { "unlocked" }.into())
            }
            "paste-pc" => {
                let text = text.replace("\r\n", "\n");
                let mut skipped = 0;
                for ch in text.chars().take(MAX_TEXT) {
                    match report(ch) {
                        Some(r) => self.tap(&r, 8, 6).await?,
                        None => skipped += 1,
                    }
                }
                Ok(if skipped > 0 {
                    format!(
                        "{skipped} characters can't be typed on a US keyboard and were skipped."
                    )
                } else {
                    String::new()
                })
            }
            _ => Err(format!("Unknown action: {name}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_the_python_backend() {
        // python: zenlink.apple.keyboard.char_usage / passcode_keys
        assert_eq!(char_usage('a'), Some((0x04, false)));
        assert_eq!(char_usage('Z'), Some((0x1D, true)));
        assert_eq!(char_usage('0'), Some((0x27, false)));
        assert_eq!(char_usage('?'), Some((0x38, true)));
        assert_eq!(char_usage('`'), Some((0x35, false)));
        assert_eq!(char_usage('é'), None);
        assert_eq!(
            passcode_keys("1234"),
            vec![vec![0x1E], vec![0x1F], vec![0x20], vec![0x21]]
        );
        assert_eq!(passcode_keys("12345").len(), 6); // 5 digits need Return
        assert_eq!(
            passcode_keys("aB\n"),
            vec![vec![0x04], vec![SHIFT, 0x05], vec![ENTER]]
        );
        assert!(passcode_keys("").is_empty());
        assert_eq!(passcode_keys(&"1".repeat(40)).len(), 33);
    }
}
