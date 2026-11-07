//! Terminal output for `hopsync setup` and `hopsync uninstall`: a short logo animation, spinners
//! with a moving shimmer, hidden password input and a closing summary.
//!
//! Animation appears only on a real UTF-8 terminal. Pipes, logs, `NO_COLOR`, `TERM=dumb`, CI and
//! `--plain` get plain lines with no escape codes, so nothing is lost when output is captured.
//! The Decision log says why this is written here instead of using a crate.

use std::fmt::Display;
use std::io::{BufRead, IsTerminal, Write};
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use rustix::termios::{self, LocalModes, OptionalActions, SpecialCodeIndex};
use zeroize::Zeroizing;

/// The logo's blue, and the lighter sky blue the gradients run to.
const BLUE: Rgb = Rgb(0x2f, 0x6f, 0xde);
const SKY: Rgb = Rgb(0x5c, 0xd4, 0xf5);
const WHITE: Rgb = Rgb(0xe8, 0xf6, 0xff);
const GREEN: Rgb = Rgb(0x3f, 0xcf, 0x8e);
const RED: Rgb = Rgb(0xf0, 0x5d, 0x5e);
const AMBER: Rgb = Rgb(0xf2, 0xb1, 0x3c);
const DIM: &str = "\x1b[2m";
const RESET: &str = "\x1b[0m";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// A step this fast would only flash, so an animated step stays on screen at least this long.
const MIN_STEP: Duration = Duration::from_millis(280);
const FRAME: Duration = Duration::from_millis(50);
const BANNER_WIDTH: usize = 19;
const HOPS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// What the environment says about the terminal. Separate from reading it, so tests can state it.
#[derive(Debug, Clone, Default)]
pub struct TermInfo {
    pub stdout_tty: bool,
    pub plain_flag: bool,
    pub no_color: bool,
    pub ci: bool,
    pub term: Option<String>,
    pub colorterm: Option<String>,
    /// The first of `LC_ALL`, `LC_CTYPE` and `LANG` that is set.
    pub locale: Option<String>,
}

impl TermInfo {
    fn from_env(plain_flag: bool) -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Self {
            stdout_tty: std::io::stdout().is_terminal(),
            plain_flag,
            no_color: var("NO_COLOR").is_some(),
            ci: var("CI").is_some(),
            term: var("TERM"),
            colorterm: var("COLORTERM"),
            locale: var("LC_ALL")
                .or_else(|| var("LC_CTYPE"))
                .or_else(|| var("LANG")),
        }
    }

    /// Animation needs a terminal that can show it: `TERM=linux` has no braille glyphs, and a
    /// locale that is not UTF-8 garbles every symbol used here.
    pub fn animate(&self) -> bool {
        let utf8 = self.locale.as_deref().is_some_and(|l| {
            let l = l.to_ascii_lowercase();
            l.contains("utf-8") || l.contains("utf8")
        });
        self.stdout_tty
            && !self.plain_flag
            && !self.no_color
            && !self.ci
            && utf8
            && !matches!(self.term.as_deref(), None | Some("dumb" | "linux"))
    }

    pub fn truecolor(&self) -> bool {
        matches!(self.colorterm.as_deref(), Some("truecolor" | "24bit"))
    }
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    let c = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t).round() as u8;
    Rgb(c(a.0, b.0), c(a.1, b.1), c(a.2, b.2))
}

/// The escape code for a colour, in true colour or the nearest of the 256-colour cube.
pub fn fg(c: Rgb, truecolor: bool) -> String {
    if truecolor {
        return format!("\x1b[38;2;{};{};{}m", c.0, c.1, c.2);
    }
    let level = |x: u8| u16::from(x) * 5 / 255;
    format!(
        "\x1b[38;5;{}m",
        16 + 36 * level(c.0) + 6 * level(c.1) + level(c.2)
    )
}

/// `text` in a blue gradient with a bright band that moves across it. `t` runs from 0 to 1 and
/// repeats; the band starts and ends off the text, so the loop has no visible jump.
pub fn shimmer(text: &str, t: f32, truecolor: bool) -> String {
    let n = text.chars().count().max(1) as f32;
    let head = t.fract() * (n + 8.0) - 4.0;
    let mut out = String::with_capacity(text.len() * 20);
    for (i, ch) in text.chars().enumerate() {
        let x = i as f32;
        let base = mix(BLUE, SKY, x / n);
        let glow = (1.0 - (x - head).abs() / 4.0).max(0.0);
        out.push_str(&fg(mix(base, WHITE, glow), truecolor));
        out.push(ch);
    }
    out.push_str(RESET);
    out
}

/// At most `max` characters of `text`, ending in `…` when cut, so a spinner line never wraps:
/// redrawing a wrapped line with `\r` leaves a trail behind.
pub fn fit(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut s: String = text.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// True when a panel row fits on one line after its 4-space indent, label column and 2-space gap.
pub fn row_fits(label_width: usize, value: &str, columns: usize) -> bool {
    4 + label_width + 2 + value.chars().count() <= columns
}

/// One banner frame at `t` from 0 to 1: three rows where a dot makes `HOPS` hops from the left box
/// to the right one. Returned without colour, one `(column, row)` for the dot.
pub fn hop_position(t: f32) -> (usize, usize) {
    let t = t.clamp(0.0, 1.0);
    let span = (BANNER_WIDTH - 1) as f32;
    let col = (t * span).round() as usize;
    let phase = (t * HOPS as f32).fract();
    // A parabola per hop, 2 rows at its top. Row 2 is the ground.
    let height = 4.0 * phase * (1.0 - phase) * 2.0;
    let row = 2usize.saturating_sub(height.round() as usize);
    (col, row)
}

/// What one byte typed into the password prompt means.
#[derive(Debug, PartialEq, Eq)]
pub enum Key {
    Continue,
    Done,
    Cancel,
}

/// The state of a password being typed. Raw mode means this code sees every byte, so it handles
/// backspace, Ctrl-U and Ctrl-C itself, and skips arrow keys rather than taking them as password.
#[derive(Default)]
pub struct PasswordLine {
    text: Zeroizing<String>,
    pending: Vec<u8>,
    in_escape: bool,
}

impl PasswordLine {
    pub fn feed(&mut self, b: u8) -> Key {
        if self.in_escape {
            // A CSI sequence ends at a byte from 0x40 to 0x7e; `[` and `O` only introduce it.
            if (0x40..=0x7e).contains(&b) && b != b'[' && b != b'O' {
                self.in_escape = false;
            }
            return Key::Continue;
        }
        match b {
            b'\r' | b'\n' => Key::Done,
            0x03 | 0x04 => Key::Cancel,
            0x1b => {
                self.in_escape = true;
                Key::Continue
            }
            0x08 | 0x7f => {
                self.pending.clear();
                self.text.pop();
                Key::Continue
            }
            0x15 => {
                self.pending.clear();
                self.text.clear();
                Key::Continue
            }
            0x00..=0x1f => Key::Continue,
            _ => {
                self.pending.push(b);
                match std::str::from_utf8(&self.pending) {
                    Ok(s) => {
                        self.text.push_str(s);
                        self.pending.clear();
                    }
                    Err(e) if e.error_len().is_some() => self.pending.clear(),
                    Err(_) => {}
                }
                Key::Continue
            }
        }
    }

    pub fn chars(&self) -> usize {
        self.text.chars().count()
    }

    pub fn take(self) -> Zeroizing<String> {
        self.text
    }
}

/// Puts the terminal back when dropped, on every way out of the password prompt.
struct RawMode<'a> {
    stdin: std::io::StdinLock<'a>,
    saved: termios::Termios,
}

impl<'a> RawMode<'a> {
    fn enter(stdin: std::io::StdinLock<'a>) -> std::io::Result<Self> {
        let saved = termios::tcgetattr(stdin.as_fd())?;
        let mut raw = saved.clone();
        raw.local_modes
            .remove(LocalModes::ECHO | LocalModes::ICANON | LocalModes::ISIG);
        raw.special_codes[SpecialCodeIndex::VMIN] = 1;
        raw.special_codes[SpecialCodeIndex::VTIME] = 0;
        termios::tcsetattr(stdin.as_fd(), OptionalActions::Now, &raw)?;
        Ok(Self { stdin, saved })
    }
}

impl Drop for RawMode<'_> {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(self.stdin.as_fd(), OptionalActions::Now, &self.saved);
    }
}

/// Undoes what an animation changed. For the Ctrl-C handler: the process exits right after.
pub fn restore_terminal() {
    if std::io::stdout().is_terminal() {
        print!("{SHOW_CURSOR}{RESET}");
        let _ = std::io::stdout().flush();
    }
}

fn width() -> usize {
    termios::tcgetwinsize(std::io::stdout().as_fd())
        .map_or(80, |w| usize::from(w.ws_col))
        .max(20)
}

/// Writes straight through: a closed pipe must not panic the process during setup.
macro_rules! out {
    ($($arg:tt)*) => {{
        let mut o = std::io::stdout().lock();
        let _ = write!(o, $($arg)*);
        let _ = o.flush();
    }};
}

pub struct Ui {
    animate: bool,
    truecolor: bool,
}

impl Ui {
    pub fn detect(plain: bool) -> Self {
        let info = TermInfo::from_env(plain);
        Self {
            animate: info.animate(),
            truecolor: info.truecolor(),
        }
    }

    fn paint(&self, c: Rgb, text: &str) -> String {
        if self.animate {
            format!("{}{text}{RESET}", fg(c, self.truecolor))
        } else {
            text.to_owned()
        }
    }

    fn dim(&self, text: &str) -> String {
        if self.animate {
            format!("{DIM}{text}{RESET}")
        } else {
            text.to_owned()
        }
    }

    /// The logo: a dot hops from one box to the other, then the name fades in beside them.
    pub fn banner(&self, tagline: &str) {
        let version = env!("CARGO_PKG_VERSION");
        if !self.animate || width() < 44 {
            out!("hopsync {version}: {tagline}\n\n");
            return;
        }
        let tc = self.truecolor;
        let ground = |lit: usize| -> String {
            let mut s = String::new();
            for col in 0..BANNER_WIDTH {
                let at_end = col == 0 || col == BANNER_WIDTH - 1;
                let c = if col <= lit {
                    SKY
                } else {
                    Rgb(0x3a, 0x4a, 0x66)
                };
                s.push_str(&fg(if at_end { BLUE } else { c }, tc));
                s.push(if at_end { '▣' } else { '·' });
            }
            s.push_str(RESET);
            s
        };
        out!("{HIDE_CURSOR}\n\n\n\n");
        const HOP_FRAMES: usize = 36;
        for f in 0..=HOP_FRAMES {
            let (col, row) = hop_position(f as f32 / HOP_FRAMES as f32);
            let mut rows = [String::new(), String::new(), ground(col)];
            let dot = format!("{}●{RESET}", fg(WHITE, tc));
            if row == 2 {
                let mut line = String::new();
                for c in 0..BANNER_WIDTH {
                    if c == col {
                        line.push_str(&dot);
                    } else {
                        let at_end = c == 0 || c == BANNER_WIDTH - 1;
                        let shade = if c <= col { SKY } else { Rgb(0x3a, 0x4a, 0x66) };
                        line.push_str(&fg(if at_end { BLUE } else { shade }, tc));
                        line.push(if at_end { '▣' } else { '·' });
                    }
                }
                line.push_str(RESET);
                rows[2] = line;
            } else {
                rows[row] = format!("{}{dot}", " ".repeat(col));
            }
            out!(
                "\x1b[3A\r\x1b[2K  {}\n\x1b[2K  {}\n\x1b[2K  {}\n",
                rows[0],
                rows[1],
                rows[2]
            );
            std::thread::sleep(Duration::from_millis(24));
        }
        let name = format!("hopsync {version}");
        let count = name.chars().count();
        for f in 0..=count * 2 {
            let shown: String = name.chars().take(f.min(count)).collect();
            let t = f as f32 / (count * 2) as f32;
            out!(
                "\x1b[3A\r\x1b[2K\n\r\x1b[2K\n\r\x1b[2K  {}   {}\n",
                ground(BANNER_WIDTH),
                shimmer(&shown, t * 0.9, tc)
            );
            std::thread::sleep(Duration::from_millis(24));
        }
        out!(
            "\x1b[1A\r\x1b[2K  {}   {}\n  {}\n{SHOW_CURSOR}\n",
            ground(BANNER_WIDTH),
            shimmer(&name, 0.0, tc),
            self.dim(tagline)
        );
    }

    /// Runs `f` under a spinner, and leaves one line with the result and how long `f` really took:
    /// the time a fast step stays on screen is for the eye, so it is not reported.
    pub fn step<T, E: Display>(
        &self,
        label: &str,
        f: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let start = Instant::now();
        let mut took = 0.0_f32;
        let result = if self.animate {
            let stop = AtomicBool::new(false);
            let line = fit(label, width().saturating_sub(16));
            let tc = self.truecolor;
            std::thread::scope(|scope| {
                let spinner = scope.spawn(|| {
                    out!("{HIDE_CURSOR}");
                    let mut frame = 0usize;
                    while !stop.load(Ordering::Relaxed) {
                        let t = frame as f32 / 34.0;
                        let glyph = SPINNER[frame % SPINNER.len()];
                        let color = mix(BLUE, SKY, (t * std::f32::consts::TAU).sin() * 0.5 + 0.5);
                        out!(
                            "\r\x1b[2K  {}{glyph}{RESET} {}",
                            fg(color, tc),
                            shimmer(&line, t, tc)
                        );
                        frame += 1;
                        std::thread::sleep(FRAME);
                    }
                });
                let r = f();
                took = start.elapsed().as_secs_f32();
                let left = MIN_STEP.saturating_sub(start.elapsed());
                std::thread::sleep(left);
                stop.store(true, Ordering::Relaxed);
                let _ = spinner.join();
                r
            })
        } else {
            let r = f();
            took = start.elapsed().as_secs_f32();
            r
        };
        let (mark, color) = match &result {
            Ok(_) => ("✓", GREEN),
            Err(_) => ("✗", RED),
        };
        if self.animate {
            // Under a tenth of a second would only print as 0.0s.
            let time = if took >= 0.1 {
                format!("  {}", self.dim(&format!("{took:.1}s")))
            } else {
                String::new()
            };
            out!(
                "\r\x1b[2K  {} {label}{time}\n{SHOW_CURSOR}",
                self.paint(color, mark)
            );
        } else {
            let word = if result.is_ok() { "done" } else { "failed" };
            out!("{label}: {word} ({took:.1}s)\n");
        }
        result
    }

    /// A step that had nothing to do.
    pub fn skipped(&self, label: &str) {
        if self.animate {
            out!("  {} {}\n", self.paint(GREEN, "✓"), self.dim(label));
        } else {
            out!("{label}\n");
        }
    }

    pub fn warn(&self, text: &str) {
        if self.animate {
            out!("  {} {text}\n", self.paint(AMBER, "!"));
        } else {
            out!("warning: {text}\n");
        }
    }

    pub fn heading(&self, text: &str) {
        if self.animate {
            out!("\n  {}\n", shimmer(text, 0.0, self.truecolor));
        } else {
            out!("\n{text}\n");
        }
    }

    /// One question with a default that Enter accepts.
    pub fn ask_line(&self, prompt: &str, default: &str) -> std::io::Result<String> {
        if self.animate {
            out!(
                "  {} {prompt} {}\n  {} ",
                self.paint(SKY, "◆"),
                self.dim(&format!("[{default}]")),
                self.paint(BLUE, "❯")
            );
        } else {
            out!("{prompt} [{default}]: ");
        }
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        let answer = line.trim();
        let answer = if answer.is_empty() { default } else { answer }.to_owned();
        // Fold the question and its input into one line with the answer, unless either wrapped:
        // then the number of lines to go back up is unknown, and the lines stay as they are.
        let question = prompt.chars().count() + default.chars().count() + 7;
        if self.animate && question < width() && line.trim_end().chars().count() + 6 < width() {
            out!(
                "\x1b[2A\r\x1b[J  {} {prompt}  {}\n",
                self.paint(GREEN, "✓"),
                self.dim(&answer)
            );
        }
        Ok(answer)
    }

    /// Yes or no. Enter means no: every use of this guards something destructive.
    pub fn confirm(&self, prompt: &str) -> std::io::Result<bool> {
        let answer = self.ask_line(prompt, "y/N")?;
        Ok(matches!(answer.to_ascii_lowercase().as_str(), "y" | "yes"))
    }

    /// Reads a password from a terminal with no echo, showing a dot per character. The dots turn
    /// green once there are `min` characters. `None` means the user pressed Ctrl-C.
    pub fn ask_password(
        &self,
        prompt: &str,
        min: usize,
    ) -> std::io::Result<Option<Zeroizing<String>>> {
        let draw = |n: usize| {
            let room = width().saturating_sub(prompt.chars().count() + 10).max(1);
            let dots = "•".repeat(n.min(room));
            let body = if n >= min {
                self.paint(GREEN, &dots)
            } else {
                self.paint(AMBER, &dots)
            };
            if self.animate {
                out!("\r\x1b[2K  {} {prompt}  {body}", self.paint(SKY, "◆"));
            } else {
                out!("\r{prompt}: {}", "*".repeat(n.min(room)));
            }
        };
        let mut raw = RawMode::enter(std::io::stdin().lock())?;
        let mut line = PasswordLine::default();
        draw(0);
        let mut byte = [0u8; 1];
        let finished = loop {
            if std::io::Read::read(&mut raw.stdin, &mut byte)? == 0 {
                break Key::Cancel;
            }
            match line.feed(byte[0]) {
                Key::Continue => draw(line.chars()),
                k => break k,
            }
        };
        drop(raw);
        // The dots go away: left in the scrollback they would show the password's length.
        if self.animate && finished == Key::Done {
            let mark = if line.chars() >= min {
                self.paint(GREEN, "✓")
            } else {
                self.paint(AMBER, "!")
            };
            out!("\r\x1b[2K  {mark} {prompt}\n");
        } else if finished == Key::Done {
            out!("\r\x1b[2K{prompt}: set\n");
        } else {
            out!("\r\x1b[2K\n");
        }
        Ok((finished == Key::Done).then(|| line.take()))
    }

    /// The closing panel: a title, then aligned `label  value` rows.
    pub fn panel(&self, title: &str, rows: &[(String, String)]) {
        let w = rows
            .iter()
            .map(|(l, _)| l.chars().count())
            .max()
            .unwrap_or(0);
        if self.animate {
            out!(
                "\n  {} {}\n\n",
                self.paint(GREEN, "●"),
                shimmer(title, 0.0, self.truecolor)
            );
            let columns = width();
            for (label, value) in rows {
                if row_fits(w, value, columns) {
                    let pad = " ".repeat(w - label.chars().count());
                    out!("    {}{pad}  {value}\n", self.dim(label));
                } else {
                    // Its own line, so a command stays whole to copy, even if the terminal wraps it.
                    out!("    {}\n{value}\n", self.dim(label));
                }
                std::thread::sleep(Duration::from_millis(28));
            }
        } else {
            out!("\n{title}\n");
            for (label, value) in rows {
                let pad = " ".repeat(w - label.chars().count());
                out!("  {label}{pad}  {value}\n");
            }
        }
        out!("\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tty() -> TermInfo {
        TermInfo {
            stdout_tty: true,
            term: Some("xterm-256color".into()),
            locale: Some("en_US.UTF-8".into()),
            ..TermInfo::default()
        }
    }

    #[test]
    fn a_row_too_long_for_the_terminal_does_not_fit() {
        assert!(row_fits(10, "12345678", 24));
        assert!(!row_fits(10, "123456789", 24));
        assert!(row_fits(10, "ünïcödé!", 24), "counts characters, not bytes");
    }

    #[test]
    fn a_utf8_terminal_animates() {
        assert!(tty().animate());
    }

    #[test]
    fn anything_that_cannot_show_animation_gets_plain_lines() {
        let cases = [
            TermInfo {
                stdout_tty: false,
                ..tty()
            },
            TermInfo {
                plain_flag: true,
                ..tty()
            },
            TermInfo {
                no_color: true,
                ..tty()
            },
            TermInfo { ci: true, ..tty() },
            TermInfo {
                term: Some("dumb".into()),
                ..tty()
            },
            TermInfo {
                term: Some("linux".into()),
                ..tty()
            },
            TermInfo {
                term: None,
                ..tty()
            },
            TermInfo {
                locale: Some("C".into()),
                ..tty()
            },
            TermInfo {
                locale: None,
                ..tty()
            },
        ];
        for case in cases {
            assert!(!case.animate(), "{case:?}");
        }
    }

    #[test]
    fn colour_falls_back_to_the_256_cube() {
        assert_eq!(fg(Rgb(255, 0, 0), true), "\x1b[38;2;255;0;0m");
        assert_eq!(fg(Rgb(255, 0, 0), false), "\x1b[38;5;196m");
    }

    #[test]
    fn shimmer_keeps_the_text_and_resets_colour() {
        let s = shimmer("héllo", 0.3, true);
        let plain: String = strip(&s);
        assert_eq!(plain, "héllo");
        assert!(s.ends_with(RESET));
    }

    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut esc = false;
        for c in s.chars() {
            match (esc, c) {
                (false, '\x1b') => esc = true,
                (true, 'm') => esc = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }

    #[test]
    fn fit_never_wraps_and_marks_the_cut() {
        assert_eq!(fit("short", 10), "short");
        assert_eq!(fit("exactly ten", 11), "exactly ten");
        assert_eq!(fit("ünïcödé wörds", 6), "ünïcö…");
    }

    #[test]
    fn the_dot_starts_and_lands_on_the_ground_at_the_boxes() {
        assert_eq!(hop_position(0.0), (0, 2));
        assert_eq!(hop_position(1.0), (BANNER_WIDTH - 1, 2));
    }

    #[test]
    fn the_dot_leaves_the_ground_once_per_hop() {
        let mut lands = 0;
        let mut was_up = false;
        for f in 0..=300 {
            let (_, row) = hop_position(f as f32 / 300.0);
            assert!(row <= 2);
            if row < 2 {
                was_up = true;
            } else if was_up {
                lands += 1;
                was_up = false;
            }
        }
        assert_eq!(lands, HOPS);
    }

    fn typed(bytes: &[u8]) -> (PasswordLine, Key) {
        let mut line = PasswordLine::default();
        let mut last = Key::Continue;
        for b in bytes {
            last = line.feed(*b);
            if last != Key::Continue {
                break;
            }
        }
        (line, last)
    }

    #[test]
    fn password_collects_text_until_enter() {
        let (line, key) = typed(b"secret\r");
        assert_eq!(key, Key::Done);
        assert_eq!(line.take().as_str(), "secret");
    }

    #[test]
    fn backspace_removes_a_whole_multibyte_character() {
        let (line, _) = typed("aé\u{7f}b\r".as_bytes());
        assert_eq!(line.take().as_str(), "ab");
    }

    #[test]
    fn multibyte_characters_count_once() {
        let (line, _) = typed("日本語".as_bytes());
        assert_eq!(line.chars(), 3);
    }

    #[test]
    fn ctrl_c_and_ctrl_d_cancel() {
        assert_eq!(typed(b"abc\x03").1, Key::Cancel);
        assert_eq!(typed(b"abc\x04").1, Key::Cancel);
    }

    #[test]
    fn ctrl_u_clears_and_arrow_keys_are_not_password() {
        let (line, _) = typed(b"abc\x15x\x1b[Ay\x1bOBz\r");
        assert_eq!(line.take().as_str(), "xyz");
    }

    #[test]
    fn invalid_utf8_is_dropped() {
        let (line, _) = typed(b"a\xffb\r");
        assert_eq!(line.take().as_str(), "ab");
    }
}
