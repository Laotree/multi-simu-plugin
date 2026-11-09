use std::io::{stdout, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseButton, MouseEventKind,
};
use crossterm::terminal::{
    self, disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use crossterm::{execute, queue};
use flate2::write::ZlibEncoder;
use flate2::Compression;

use crate::target::{Action, SpecialKey, Target};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Graphics {
    Kitty,
    Unicode,
}

#[derive(Clone, Copy)]
struct Layout {
    cell_w: usize,
    cell_h: usize,
    col0: u16,
    row0: u16,
    img_cols: u16,
    img_rows: u16,
    disp_w: usize,
    disp_h: usize,
}

struct KittyState {
    frame: u64,
    file: bool,
}

fn fit(src_w: usize, src_h: usize, area_w: usize, area_h: usize) -> (usize, usize) {
    if src_w == 0 || src_h == 0 || area_w == 0 || area_h == 0 {
        return (1, 1);
    }
    let scale = (area_w as f64 / src_w as f64).min(area_h as f64 / src_h as f64);
    (
        ((src_w as f64 * scale).round() as usize).max(1),
        ((src_h as f64 * scale).round() as usize).max(1),
    )
}

pub(crate) fn rex_cell_size() -> Option<(usize, usize)> {
    let session = std::env::var("REX_SESSION").ok()?;
    let block = std::env::var("REX_BLOCK").ok()?;
    let out = std::process::Command::new("rex")
        .args(["block", "call", "-s", &session, "-b", &block, "size"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let w = v.get("cell_width_px")?.as_u64()? as usize;
    let h = v.get("cell_height_px")?.as_u64()? as usize;
    if w > 0 && h > 0 {
        Some((w, h))
    } else {
        None
    }
}

fn cell_size(cols: u16, rows: u16, fixed: Option<(usize, usize)>) -> (usize, usize) {
    if let Some((w, h)) = fixed {
        return (w, h);
    }
    if let Ok(size) = terminal::window_size() {
        if size.columns > 0 && size.rows > 0 && size.width > 0 && size.height > 0 {
            return (
                (size.width as usize / size.columns as usize).max(1),
                (size.height as usize / size.rows as usize).max(1),
            );
        }
    }
    let _ = (cols, rows);
    (8, 16)
}

fn layout(
    cols: u16,
    rows: u16,
    src_w: usize,
    src_h: usize,
    fixed: Option<(usize, usize)>,
) -> Layout {
    let (cell_w, cell_h) = cell_size(cols, rows, fixed);
    let area_rows = rows.saturating_sub(1) as usize;
    let (display_w, display_h) = fit(src_w, src_h, cols as usize * cell_w, area_rows * cell_h);
    let img_cols = (display_w.div_ceil(cell_w).max(1) as u16).min(cols);
    let img_rows = (display_h.div_ceil(cell_h).max(1) as u16).min(area_rows as u16);
    Layout {
        cell_w,
        cell_h,
        col0: (cols.saturating_sub(img_cols)) / 2,
        // 0-based rows above the toolbar; the toolbar owns the last row.
        row0: ((area_rows as u16).saturating_sub(img_rows)) / 2,
        img_cols,
        img_rows,
        disp_w: display_w,
        disp_h: display_h,
    }
}

fn draw_kitty(
    out: &mut impl Write,
    rgba: &[u8],
    w: usize,
    h: usize,
    layout: Layout,
    state: &mut KittyState,
) -> std::io::Result<()> {
    let id = if state.frame.is_multiple_of(2) { 1 } else { 2 };
    let old = if id == 1 { 2 } else { 1 };
    queue!(out, MoveTo(layout.col0, layout.row0))?;
    if state.file {
        let path = format!(
            "/tmp/tty-graphics-protocol-multi-simu-{}-{}.rgba",
            std::process::id(),
            state.frame
        );
        std::fs::write(&path, rgba)?;
        let payload = base64::engine::general_purpose::STANDARD.encode(path.as_bytes());
        queue!(
            out,
            crossterm::style::Print(format!(
                "\x1b_Ga=T,f=32,s={w},v={h},i={id},p=1,C=1,q=1,t=t;{payload}\x1b\\"
            )),
            crossterm::style::Print(format!("\x1b_Ga=d,d=I,i={old},q=2\x1b\\"))
        )?;
    } else {
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::fast());
        enc.write_all(rgba)?;
        let z = enc.finish()?;
        let payload = base64::engine::general_purpose::STANDARD.encode(z);
        queue!(
            out,
            crossterm::style::Print(format!(
                "\x1b_Ga=T,f=32,s={w},v={h},i={id},p=1,C=1,q=2,o=z;{payload}\x1b\\"
            )),
            crossterm::style::Print(format!("\x1b_Ga=d,d=I,i={old},q=2\x1b\\"))
        )?;
    }
    state.frame += 1;
    Ok(())
}

fn unicode_frame(
    out: &mut impl Write,
    rgba: &[u8],
    w: usize,
    h: usize,
    area_cols: u16,
    area_rows: u16,
) -> std::io::Result<()> {
    let rows = (h / 2).min(area_rows as usize);
    let top_pad = (area_rows as usize).saturating_sub(rows) / 2;
    for row in 0..rows {
        let y0 = (row * 2).min(h.saturating_sub(1));
        let y1 = (row * 2 + 1).min(h.saturating_sub(1));
        let mut line = String::with_capacity(area_cols as usize * 24);
        line.push_str(&format!("\x1b[{};1H", top_pad + row + 1));
        for x in 0..(area_cols as usize).min(w) {
            let a = (y0 * w + x) * 4;
            let b = (y1 * w + x) * 4;
            line.push_str(&format!(
                "\x1b[38;2;{};{};{};48;2;{};{};{}m▀",
                rgba[a],
                rgba[a + 1],
                rgba[a + 2],
                rgba[b],
                rgba[b + 1],
                rgba[b + 2]
            ));
        }
        line.push_str("\x1b[0m");
        queue!(out, crossterm::style::Print(line))?;
    }
    Ok(())
}

fn draw_toolbar(out: &mut impl Write, text: &str, cols: u16, rows: u16) -> std::io::Result<()> {
    // Never write into the last cell: a full-width line can wrap and leave a
    // second copy of the toolbar on screen.
    let limit = cols.saturating_sub(1) as usize;
    let text: String = text.chars().take(limit).collect();
    queue!(
        out,
        MoveTo(0, rows.saturating_sub(1)),
        Clear(ClearType::CurrentLine),
        crossterm::style::Print(text)
    )?;
    Ok(())
}

const BUTTONS: &[(&str, Action)] = &[
    ("Home", Action::Home),
    ("Switcher", Action::Switcher),
    ("Back", Action::Back),
    ("Lock", Action::Lock),
    ("Center", Action::Center),
    ("Notify", Action::Notify),
    ("Vol-", Action::VolDown),
    ("Vol+", Action::VolUp),
];

struct Toolbar {
    text: String,
    spans: Vec<(u16, u16, Action)>,
    quit: (u16, u16),
}

fn toolbar(cols: u16) -> Toolbar {
    let label = "[Exit]";
    let quit_len = label.len() as u16;
    let quit_start = cols.saturating_sub(quit_len + 1);
    let mut text = String::new();
    let mut spans = Vec::new();
    let mut x = 0u16;
    for (name, action) in BUTTONS {
        let piece = format!("[{name}]");
        let len = piece.len() as u16;
        if x + len + 1 > quit_start {
            break;
        }
        spans.push((x, x + len, *action));
        text.push_str(&piece);
        x += len;
        if x < quit_start {
            text.push(' ');
            x += 1;
        }
    }
    while (text.len() as u16) < quit_start {
        text.push(' ');
    }
    let start = text.len() as u16;
    text.push_str(label);
    Toolbar {
        text,
        spans,
        quit: (start, start + quit_len),
    }
}

struct Frame {
    rgba: Vec<u8>,
    w: usize,
    h: usize,
    layout: Layout,
    graphics: Graphics,
    toolbar: String,
    cols: u16,
    rows: u16,
    marker: Option<(u16, u16)>,
}

fn render_loop(rx: Receiver<Frame>, stop: Arc<AtomicBool>, file_medium: bool) {
    let mut kitty = KittyState {
        frame: 0,
        file: file_medium,
    };
    let mut last_size = (0u16, 0u16);
    let mut last_toolbar = String::new();
    let mut last_marker: Option<(u16, u16)> = None;
    let mut out = stdout();
    while let Ok(frame) = rx.recv() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let size = (frame.cols, frame.rows);
        if size != last_size {
            let _ = write!(out, "\x1b_Ga=d,d=A,q=2\x1b\\\x1b[2J");
            last_size = size;
        }
        match frame.graphics {
            Graphics::Kitty => {
                let _ = draw_kitty(
                    &mut out,
                    &frame.rgba,
                    frame.w,
                    frame.h,
                    frame.layout,
                    &mut kitty,
                );
            }
            Graphics::Unicode => {
                let _ = unicode_frame(
                    &mut out,
                    &frame.rgba,
                    frame.w,
                    frame.h,
                    frame.layout.img_cols.max(1),
                    frame.rows.saturating_sub(1),
                );
            }
        }
        if frame.toolbar != last_toolbar {
            let _ = draw_toolbar(&mut out, &frame.toolbar, frame.cols, frame.rows);
            last_toolbar = frame.toolbar;
        }
        if frame.marker != last_marker {
            if let Some((col, row)) = frame.marker {
                let _ = write!(
                    out,
                    "\x1b[{};{}H\x1b[1;31m+\x1b[0m",
                    row.saturating_add(1),
                    col.saturating_add(1)
                );
            } else if let Some((col, row)) = last_marker {
                // A space leaves the image behind visible.
                let _ = write!(
                    out,
                    "\x1b[{};{}H ",
                    row.saturating_add(1),
                    col.saturating_add(1)
                );
            }
            last_marker = frame.marker;
        }
        let _ = out.flush();
    }
}

fn special_key(code: KeyCode) -> Option<SpecialKey> {
    Some(match code {
        KeyCode::Enter => SpecialKey::Enter,
        KeyCode::Esc => SpecialKey::Esc,
        KeyCode::Backspace => SpecialKey::Backspace,
        KeyCode::Tab => SpecialKey::Tab,
        KeyCode::Delete => SpecialKey::Delete,
        KeyCode::Up => SpecialKey::Up,
        KeyCode::Down => SpecialKey::Down,
        KeyCode::Left => SpecialKey::Left,
        KeyCode::Right => SpecialKey::Right,
        KeyCode::Home => SpecialKey::Home,
        KeyCode::End => SpecialKey::End,
        KeyCode::PageUp => SpecialKey::PageUp,
        KeyCode::PageDown => SpecialKey::PageDown,
        _ => return None,
    })
}

#[derive(Clone, Copy)]
struct Metrics {
    cols: u16,
    rows: u16,
    cell_w: usize,
    cell_h: usize,
}

fn parse_metrics(buf: &[u8]) -> Option<Metrics> {
    let text = String::from_utf8_lossy(buf);
    let mut cols = None;
    let mut rows = None;
    let mut cell_w = None;
    let mut cell_h = None;
    let mut win_w = None;
    let mut win_h = None;
    let mut rest = text.as_ref();
    while let Some(i) = rest.find("\x1b[") {
        let after = &rest[i + 2..];
        let Some(end) = after.find('t') else { break };
        let body = &after[..end];
        let parts: Vec<&str> = body.split(';').collect();
        if let Some(kind) = parts.first().and_then(|x| x.parse::<u32>().ok()) {
            let nums: Vec<u32> = parts
                .iter()
                .skip(1)
                .filter_map(|x| x.parse::<u32>().ok())
                .collect();
            match kind {
                8 if nums.len() >= 2 => {
                    rows = Some(nums[0] as u16);
                    cols = Some(nums[1] as u16);
                }
                4 if nums.len() >= 2 => {
                    win_h = Some(nums[0] as usize);
                    win_w = Some(nums[1] as usize);
                }
                6 if nums.len() >= 2 => {
                    cell_h = Some(nums[0] as usize);
                    cell_w = Some(nums[1] as usize);
                }
                _ => {}
            }
        }
        rest = &after[end + 1..];
    }
    let cell_w = cell_w?;
    let cell_h = cell_h?;
    if cell_w == 0 || cell_h == 0 {
        return None;
    }
    let cols = cols.or_else(|| win_w.map(|w| (w / cell_w).max(1) as u16))?;
    let rows = rows.or_else(|| win_h.map(|h| (h / cell_h).max(1) as u16))?;
    if cols == 0 || rows == 0 {
        return None;
    }
    Some(Metrics {
        cols,
        rows,
        cell_w,
        cell_h,
    })
}

fn query_metrics() -> Option<Metrics> {
    use std::io::{Read, Write};
    let mut out = stdout();
    let _ = out.write_all(b"\x1b[18t\x1b[14t\x1b[16t");
    let _ = out.flush();
    let mut buf = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut pfd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        let n = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) };
        if n <= 0 {
            break;
        }
        let mut chunk = [0u8; 512];
        match std::io::stdin().read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if let Some(m) = parse_metrics(&buf) {
                    return Some(m);
                }
            }
        }
    }
    parse_metrics(&buf)
}

fn fallback_metrics() -> Metrics {
    let (cols, rows) = terminal::size().unwrap_or((80, 24));
    let cell = rex_cell_size()
        .or_else(|| {
            terminal::window_size().ok().and_then(|s| {
                if s.columns > 0 && s.rows > 0 && s.width > 0 && s.height > 0 {
                    Some((
                        (s.width as usize / s.columns as usize).max(1),
                        (s.height as usize / s.rows as usize).max(1),
                    ))
                } else {
                    None
                }
            })
        })
        .unwrap_or((8, 16));
    Metrics {
        cols,
        rows,
        cell_w: cell.0,
        cell_h: cell.1,
    }
}

pub fn run(
    target: &mut dyn Target,
    _label: &str,
    graphics: Graphics,
    fps: u32,
) -> Result<(), String> {
    let (mut src_w, mut src_h) = target.size();
    let mut out = stdout();
    enable_raw_mode().map_err(|e| e.to_string())?;
    execute!(out, EnterAlternateScreen, Hide, EnableMouseCapture).map_err(|e| e.to_string())?;

    let file_medium = std::env::var("TERM_PROGRAM").as_deref() == Ok("rex")
        || std::env::var("MULTI_SIM_FILE_MEDIUM").as_deref() == Ok("1");
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx): (SyncSender<Frame>, Receiver<Frame>) = sync_channel(1);
    let stop_thread = stop.clone();
    std::thread::spawn(move || render_loop(rx, stop_thread, file_medium));

    let mut metrics = query_metrics().unwrap_or_else(fallback_metrics);
    let mut last_pty = terminal::size().unwrap_or((metrics.cols, metrics.rows));
    let interval = Duration::from_millis((1000 / fps.max(1)) as u64);
    let mut last_draw = Instant::now() - interval;
    let mut dragging = false;
    let mut last_move = Instant::now();
    let mut last_rgba: Option<(Vec<u8>, usize, usize)> = None;
    let mut last_size = (0u16, 0u16);
    let mut last_toolbar = String::new();
    let mut marker: Option<(u16, u16, Instant)> = None;
    let mut last_marker_sent: Option<(u16, u16)> = None;

    let result = (|| -> Result<(), String> {
        loop {
            while event::poll(Duration::from_millis(0)).map_err(|e| e.to_string())? {
                match event::read().map_err(|e| e.to_string())? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        if key.code == KeyCode::F(10)
                            || (key.modifiers.contains(KeyModifiers::CONTROL)
                                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')))
                        {
                            return Ok(());
                        }
                        match key.code {
                            KeyCode::F(1) => {
                                target.action(Action::Home);
                                continue;
                            }
                            KeyCode::F(2) => {
                                target.action(Action::Switcher);
                                continue;
                            }
                            KeyCode::F(3) => {
                                target.action(Action::Back);
                                continue;
                            }
                            KeyCode::F(4) => {
                                target.action(Action::Lock);
                                continue;
                            }
                            KeyCode::F(5) => {
                                target.action(Action::Center);
                                continue;
                            }
                            KeyCode::F(6) => {
                                target.action(Action::Notify);
                                continue;
                            }
                            KeyCode::F(7) => {
                                target.action(Action::VolDown);
                                continue;
                            }
                            KeyCode::F(8) => {
                                target.action(Action::VolUp);
                                continue;
                            }
                            _ => {}
                        }
                        if let Some(special) = special_key(key.code) {
                            target.special(special);
                        } else if let KeyCode::Char(c) = key.code {
                            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                            let shift = key.modifiers.contains(KeyModifiers::SHIFT);
                            target.char(c, shift, ctrl);
                        }
                    }
                    Event::Mouse(mouse) => {
                        let (cols, rows) = (metrics.cols, metrics.rows);
                        if rows < 2 {
                            continue;
                        }
                        if mouse.row == rows - 1 {
                            if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
                                let tb = toolbar(cols);
                                if mouse.column >= tb.quit.0 && mouse.column < tb.quit.1 {
                                    return Ok(());
                                }
                                if let Some((_, _, action)) = tb
                                    .spans
                                    .iter()
                                    .find(|(a, b, _)| mouse.column >= *a && mouse.column < *b)
                                {
                                    target.action(*action);
                                }
                            }
                            continue;
                        }
                        let l = layout(
                            cols,
                            rows,
                            src_w,
                            src_h,
                            Some((metrics.cell_w, metrics.cell_h)),
                        );
                        let inside = mouse.column >= l.col0
                            && mouse.column < l.col0 + l.img_cols
                            && mouse.row >= l.row0
                            && mouse.row < l.row0 + l.img_rows;
                        if !inside {
                            continue;
                        }
                        let x = ((mouse.column - l.col0) as f64 * l.cell_w as f64
                            / l.disp_w as f64)
                            .clamp(0.0, 1.0);
                        let y = ((mouse.row - l.row0) as f64 * l.cell_h as f64 / l.disp_h as f64)
                            .clamp(0.0, 1.0);
                        match mouse.kind {
                            MouseEventKind::Down(MouseButton::Left) => {
                                dragging = true;
                                marker = Some((mouse.column, mouse.row, Instant::now()));
                                let _ = std::fs::OpenOptions::new()
                                    .create(true)
                                    .append(true)
                                    .open("/tmp/multi-simu.log")
                                    .map(|mut f| {
                                        use std::io::Write as _;
                                        let _ = writeln!(
                                            f,
                                            "click col={} row={} x={x:.3} y={y:.3}",
                                            mouse.column, mouse.row
                                        );
                                    });
                                target.touch(x, y, true);
                            }
                            MouseEventKind::Drag(MouseButton::Left) if dragging => {
                                if last_move.elapsed() >= Duration::from_millis(30) {
                                    target.touch(x, y, true);
                                    last_move = Instant::now();
                                }
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                if dragging {
                                    target.touch(x, y, false);
                                    dragging = false;
                                }
                            }
                            MouseEventKind::ScrollUp => {
                                target.swipe(x, y, x, (y - 0.2).max(0.01));
                            }
                            MouseEventKind::ScrollDown => {
                                target.swipe(x, y, x, (y + 0.2).min(0.99));
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }

            if last_draw.elapsed() >= interval {
                last_draw = Instant::now();
                let pty = terminal::size().unwrap_or(last_pty);
                if pty != last_pty {
                    last_pty = pty;
                    if let Some(m) = query_metrics() {
                        metrics = m;
                    }
                }
                let (cols, rows) = (metrics.cols, metrics.rows);
                if rows < 2 {
                    continue;
                }
                let size_changed = (cols, rows) != last_size;
                if size_changed {
                    let area_w = cols as usize * metrics.cell_w;
                    let area_h = rows.saturating_sub(1) as usize * metrics.cell_h;
                    target.resize(area_w, area_h);
                    let (w, h) = target.size();
                    if (w, h) != (src_w, src_h) {
                        src_w = w;
                        src_h = h;
                        last_rgba = None;
                    }
                }
                let l = layout(
                    cols,
                    rows,
                    src_w,
                    src_h,
                    Some((metrics.cell_w, metrics.cell_h)),
                );
                let tb = toolbar(cols);
                let toolbar_changed = tb.text != last_toolbar;

                if let Some((_, _, t)) = marker {
                    if t.elapsed() >= Duration::from_millis(350) {
                        marker = None;
                    }
                }
                let marker_cell = marker.map(|(c, r, _)| (c, r));
                if marker_cell != last_marker_sent {
                    if let Some((rgba, w, h)) = &last_rgba {
                        let _ = tx.try_send(Frame {
                            rgba: rgba.clone(),
                            w: *w,
                            h: *h,
                            layout: l,
                            graphics,
                            toolbar: tb.text.clone(),
                            cols,
                            rows,
                            marker: marker_cell,
                        });
                    }
                    last_marker_sent = marker_cell;
                }

                if size_changed || toolbar_changed {
                    if let Some((rgba, w, h)) = &last_rgba {
                        let _ = tx.try_send(Frame {
                            rgba: rgba.clone(),
                            w: *w,
                            h: *h,
                            layout: l,
                            graphics,
                            toolbar: tb.text.clone(),
                            cols,
                            rows,
                            marker: marker_cell,
                        });
                    }
                }

                match graphics {
                    Graphics::Kitty => {
                        if let Some(rgba) = target.frame_rgba(l.disp_w, l.disp_h) {
                            last_rgba = Some((rgba.clone(), l.disp_w, l.disp_h));
                            match tx.try_send(Frame {
                                rgba,
                                w: l.disp_w,
                                h: l.disp_h,
                                layout: l,
                                graphics,
                                toolbar: tb.text.clone(),
                                cols,
                                rows,
                                marker: marker_cell,
                            }) {
                                Ok(()) | Err(TrySendError::Full(_)) => {}
                                Err(TrySendError::Disconnected(_)) => break,
                            }
                        }
                    }
                    Graphics::Unicode => {
                        let area_rows = rows.saturating_sub(1);
                        let (uw, uh) = fit(src_w, src_h, cols as usize, area_rows as usize * 2);
                        let uh = (uh / 2).max(1);
                        if let Some(rgba) = target.frame_rgba(uw, uh * 2) {
                            last_rgba = Some((rgba.clone(), uw, uh * 2));
                            match tx.try_send(Frame {
                                rgba,
                                w: uw,
                                h: uh * 2,
                                layout: l,
                                graphics,
                                toolbar: tb.text.clone(),
                                cols,
                                rows,
                                marker: marker_cell,
                            }) {
                                Ok(()) | Err(TrySendError::Full(_)) => {}
                                Err(TrySendError::Disconnected(_)) => break,
                            }
                        }
                    }
                }
                last_size = (cols, rows);
                last_toolbar = tb.text;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    })();

    target.shutdown();
    stop.store(true, Ordering::Relaxed);
    // Give the render thread a moment to finish its current write.
    std::thread::sleep(Duration::from_millis(60));
    let _ = execute!(
        stdout(),
        crossterm::style::Print("\x1b_Ga=d,d=A,q=2\x1b\\"),
        DisableMouseCapture,
        Show,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_and_cell_size() {
        let buf = b"\x1b[8;63;104t\x1b[6;28;14t";
        let m = parse_metrics(buf).expect("metrics");
        assert_eq!((m.cols, m.rows), (104, 63));
        assert_eq!((m.cell_w, m.cell_h), (14, 28));
    }

    #[test]
    fn derives_size_from_window_pixels() {
        let buf = b"\x1b[4;1764;1456t\x1b[6;28;14t";
        let m = parse_metrics(buf).expect("metrics");
        assert_eq!((m.cols, m.rows), (104, 63));
    }

    #[test]
    fn fit_keeps_aspect() {
        assert_eq!(fit(1206, 2622, 1000, 600), (276, 600));
        assert_eq!(fit(1206, 2622, 0, 0), (1, 1));
    }
}
